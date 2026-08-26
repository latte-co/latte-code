//! Append-only per-Session conversation storage.
use latte_core::{RunId, ThreadId, TranscriptEntry, TranscriptEntryId, TranscriptKind};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct ConversationStore {
    workspace_dir: PathBuf,
    workspace_key: String,
    io: Mutex<()>,
}

impl ConversationStore {
    pub(crate) fn open(root: &Path, workspace_key: &str) -> Result<Self, String> {
        if workspace_key.is_empty()
            || !workspace_key
                .bytes()
                .all(|value| value.is_ascii_alphanumeric() || value == b'-')
        {
            return Err("invalid conversation workspace key".into());
        }
        ensure_private_directory(root)?;
        let workspace_dir = root.join(workspace_key);
        ensure_private_directory(&workspace_dir)?;
        Ok(Self {
            workspace_dir,
            workspace_key: workspace_key.into(),
            io: Mutex::new(()),
        })
    }

    pub(crate) fn sync(
        &self,
        thread_id: ThreadId,
        created_at_ms: u64,
        entries: &[TranscriptEntry],
    ) -> Result<(), String> {
        let _guard = self.io.lock().expect("conversation mutex poisoned");
        let path = self.workspace_dir.join(format!("{thread_id}.jsonl"));
        let mut file = open_session_file(&path)?;
        // Different Latte Code processes have distinct in-memory mutexes but may
        // drain the same transactional outbox. Serialize the repair/read/append
        // sequence on the file itself so duplicate drains remain idempotent.
        file.lock().map_err(io_error)?;
        if file.metadata().map_err(io_error)?.len() == 0 {
            if entries.is_empty() {
                return Err(
                    "conversation file is missing and the durable outbox has no recovery records"
                        .into(),
                );
            }
            let header = json!({
                "record": "session",
                "format_version": 1,
                "session_id": thread_id,
                "workspace_id": self.workspace_key,
                "created_at_ms": created_at_ms,
            });
            write_record(&mut file, &header)?;
            file.sync_data().map_err(io_error)?;
        }
        let (existing, _) = repair_and_read(&mut file, thread_id, &self.workspace_key)?;
        let mut last_sequence = existing.keys().next_back().copied().unwrap_or(0);
        file.seek(SeekFrom::End(0)).map_err(io_error)?;
        for entry in entries {
            if let Some(existing_id) = existing.get(&entry.sequence) {
                if existing_id != &entry.entry_id.to_string() {
                    return Err(format!(
                        "conversation sequence {} has a different entry id",
                        entry.sequence
                    ));
                }
                continue;
            }
            if entry.sequence <= last_sequence {
                return Err(format!(
                    "conversation sequence is not increasing: previous {last_sequence}, found {}",
                    entry.sequence
                ));
            }
            let record = json!({
                "record": "entry",
                "format_version": 1,
                "entry_id": entry.entry_id,
                "seq": entry.sequence,
                "run_id": entry.run_id,
                "created_at_ms": entry.created_at_ms,
                "kind": entry.kind,
                "content": entry.text,
                "payload": entry.payload,
                "source_key": entry.source_key,
            });
            write_record(&mut file, &record)?;
            last_sequence = entry.sequence;
        }
        file.sync_data().map_err(io_error)
    }

    pub(crate) fn read(&self, thread_id: ThreadId) -> Result<Vec<TranscriptEntry>, String> {
        let _guard = self.io.lock().expect("conversation mutex poisoned");
        let path = self.workspace_dir.join(format!("{thread_id}.jsonl"));
        let mut file = open_session_file(&path)?;
        file.lock().map_err(io_error)?;
        let (_, entries) = repair_and_read(&mut file, thread_id, &self.workspace_key)?;
        Ok(entries)
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    if path.exists()
        && fs::symlink_metadata(path)
            .map_err(io_error)?
            .file_type()
            .is_symlink()
    {
        return Err(format!(
            "conversation directory is a symlink: {}",
            path.display()
        ));
    }
    fs::create_dir_all(path).map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    Ok(())
}

fn open_session_file(path: &Path) -> Result<File, String> {
    if path.exists()
        && fs::symlink_metadata(path)
            .map_err(io_error)?
            .file_type()
            .is_symlink()
    {
        return Err(format!(
            "conversation file is a symlink: {}",
            path.display()
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(io_error)?;
    }
    Ok(file)
}

#[allow(clippy::too_many_lines)]
fn repair_and_read(
    file: &mut File,
    thread_id: ThreadId,
    workspace_key: &str,
) -> Result<(BTreeMap<u64, String>, Vec<TranscriptEntry>), String> {
    let length = file.metadata().map_err(io_error)?.len();
    if length > MAX_FILE_BYTES {
        return Err("conversation file exceeds the bounded repair limit".into());
    }
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut bytes = Vec::with_capacity(usize::try_from(length).map_err(|_| "file too large")?);
    file.read_to_end(&mut bytes).map_err(io_error)?;
    if !bytes.ends_with(b"\n") {
        let last_newline = bytes
            .iter()
            .rposition(|value| *value == b'\n')
            .ok_or_else(|| "conversation header is torn".to_owned())?;
        let repaired = u64::try_from(last_newline + 1).map_err(|_| "repair offset overflow")?;
        file.set_len(repaired).map_err(io_error)?;
        file.sync_data().map_err(io_error)?;
        bytes.truncate(last_newline + 1);
    }
    let mut lines = bytes
        .split(|value| *value == b'\n')
        .filter(|line| !line.is_empty());
    let header: Value = serde_json::from_slice(
        lines
            .next()
            .ok_or_else(|| "conversation header is missing".to_owned())?,
    )
    .map_err(json_error)?;
    if header.get("record").and_then(Value::as_str) != Some("session")
        || header.get("format_version").and_then(Value::as_u64) != Some(1)
        || header.get("session_id").and_then(Value::as_str) != Some(&thread_id.to_string())
        || header.get("workspace_id").and_then(Value::as_str) != Some(workspace_key)
    {
        return Err("conversation header identity mismatch".into());
    }
    let mut entries = BTreeMap::new();
    let mut transcript = Vec::new();
    let mut previous = 0;
    for line in lines {
        if line.len() > MAX_RECORD_BYTES {
            return Err("conversation record exceeds the line limit".into());
        }
        let record: Value = serde_json::from_slice(line).map_err(json_error)?;
        if record.get("record").and_then(Value::as_str) != Some("entry")
            || record.get("format_version").and_then(Value::as_u64) != Some(1)
        {
            return Err("invalid conversation record type".into());
        }
        let sequence = record
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or_else(|| "conversation record has no sequence".to_owned())?;
        let entry_id = record
            .get("entry_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "conversation record has no entry id".to_owned())?;
        if sequence <= previous || entries.insert(sequence, entry_id.into()).is_some() {
            return Err("conversation records are not a strictly increasing sequence".into());
        }
        previous = sequence;
        let entry_id = uuid::Uuid::parse_str(entry_id)
            .map(TranscriptEntryId::from_uuid)
            .map_err(|error| format!("invalid conversation entry id: {error}"))?;
        let run_id = record
            .get("run_id")
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| "invalid conversation run id".to_owned())
                    .and_then(|value| {
                        uuid::Uuid::parse_str(value)
                            .map(RunId::from_uuid)
                            .map_err(|error| format!("invalid conversation run id: {error}"))
                    })
            })
            .transpose()?;
        let kind: TranscriptKind = serde_json::from_value(
            record
                .get("kind")
                .cloned()
                .ok_or_else(|| "conversation record has no kind".to_owned())?,
        )
        .map_err(json_error)?;
        transcript.push(TranscriptEntry {
            entry_id,
            sequence,
            run_id,
            kind,
            text: record
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "conversation record has no content".to_owned())?
                .into(),
            payload: record
                .get("payload")
                .filter(|value| !value.is_null())
                .cloned(),
            source_key: record
                .get("source_key")
                .and_then(Value::as_str)
                .ok_or_else(|| "conversation record has no source key".to_owned())?
                .into(),
            created_at_ms: record
                .get("created_at_ms")
                .and_then(Value::as_u64)
                .ok_or_else(|| "conversation record has no creation time".to_owned())?,
        });
    }
    Ok((entries, transcript))
}

fn write_record(file: &mut File, record: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(record).map_err(json_error)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err("conversation record exceeds the line limit".into());
    }
    file.write_all(&bytes).map_err(io_error)?;
    file.write_all(b"\n").map_err(io_error)
}

#[allow(clippy::needless_pass_by_value)]
fn io_error(error: std::io::Error) -> String {
    format!("conversation storage failure: {error}")
}

#[allow(clippy::needless_pass_by_value)]
fn json_error(error: serde_json::Error) -> String {
    format!("invalid conversation JSONL: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use latte_core::{IdSource, SystemIdSource, TranscriptEntryId, TranscriptKind};

    fn entry(sequence: u64, text: &str) -> TranscriptEntry {
        TranscriptEntry {
            entry_id: TranscriptEntryId::from_uuid(SystemIdSource::default().next_uuid_v7()),
            sequence,
            run_id: None,
            kind: TranscriptKind::User,
            text: text.into(),
            payload: None,
            source_key: format!("test:{sequence}"),
            created_at_ms: sequence,
        }
    }

    fn session_path(root: &Path, workspace_key: &str, thread_id: ThreadId) -> PathBuf {
        root.join(workspace_key).join(format!("{thread_id}.jsonl"))
    }

    fn header(thread_id: ThreadId, workspace_key: &str) -> Value {
        json!({
            "record": "session",
            "format_version": 1,
            "session_id": thread_id,
            "workspace_id": workspace_key,
            "created_at_ms": 1,
        })
    }

    fn record(entry: &TranscriptEntry) -> Value {
        json!({
            "record": "entry",
            "format_version": 1,
            "entry_id": entry.entry_id,
            "seq": entry.sequence,
            "run_id": entry.run_id,
            "created_at_ms": entry.created_at_ms,
            "kind": entry.kind,
            "content": entry.text,
            "payload": entry.payload,
            "source_key": entry.source_key,
        })
    }

    fn overwrite_records(path: &Path, header: &Value, records: &[Value]) {
        let mut bytes = serde_json::to_vec(header).unwrap();
        bytes.push(b'\n');
        for record in records {
            bytes.extend(serde_json::to_vec(record).unwrap());
            bytes.push(b'\n');
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn sync_repairs_only_a_torn_final_line_and_preserves_contiguous_entries() {
        let root = tempfile::tempdir().unwrap();
        let store = ConversationStore::open(root.path(), "workspace-abc").unwrap();
        let thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let entries = vec![entry(1, "first"), entry(2, "second")];
        store.sync(thread_id, 1, &entries).unwrap();
        let path = root
            .path()
            .join("workspace-abc")
            .join(format!("{thread_id}.jsonl"));
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"record":"entry","seq":3"#)
            .unwrap();

        let mut completed = entries;
        completed.push(entry(3, "third"));
        store.sync(thread_id, 1, &completed).unwrap();
        let bytes = fs::read(path).unwrap();
        assert!(bytes.ends_with(b"\n"));
        let records = bytes
            .split(|value| *value == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 4);
        assert_eq!(records[3]["content"], "third");
        assert_eq!(records[3]["seq"], 3);
    }

    #[test]
    fn separate_store_handles_serialize_on_the_session_file() {
        use std::{
            sync::{Arc, Barrier},
            thread,
            time::Duration,
        };

        let root = tempfile::tempdir().unwrap();
        let first = ConversationStore::open(root.path(), "workspace-abc").unwrap();
        let second = ConversationStore::open(root.path(), "workspace-abc").unwrap();
        let thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
        let entries = vec![entry(1, "first"), entry(2, "second")];
        first.sync(thread_id, 1, &entries[..1]).unwrap();
        let path = root
            .path()
            .join("workspace-abc")
            .join(format!("{thread_id}.jsonl"));
        let locked = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        locked.lock().unwrap();

        let started = Arc::new(Barrier::new(2));
        let worker_started = Arc::clone(&started);
        let worker = thread::spawn(move || {
            worker_started.wait();
            second.sync(thread_id, 1, &entries)
        });
        started.wait();
        thread::sleep(Duration::from_millis(25));
        assert!(!worker.is_finished());
        drop(locked);
        worker.join().unwrap().unwrap();

        assert_eq!(first.read(thread_id).unwrap().len(), 2);
    }

    #[test]
    fn rejects_unsafe_paths_and_bounded_storage_violations() {
        let root = tempfile::tempdir().unwrap();
        assert!(ConversationStore::open(root.path(), "workspace/unsafe").is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path(), root.path().join("linked-root")).unwrap();
            assert!(ConversationStore::open(&root.path().join("linked-root"), "safe").is_err());
        }

        let store = ConversationStore::open(root.path(), "workspace-safe").unwrap();
        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        store.sync(thread_id, 1, &[entry(1, "first")]).unwrap();

        #[cfg(unix)]
        {
            let path = session_path(root.path(), "workspace-safe", thread_id);
            let linked_thread = ThreadId::from_uuid(ids.next_uuid_v7());
            std::os::unix::fs::symlink(
                &path,
                session_path(root.path(), "workspace-safe", linked_thread),
            )
            .unwrap();
            assert!(store.read(linked_thread).is_err());
        }

        let oversized_thread = ThreadId::from_uuid(ids.next_uuid_v7());
        let oversized_path = session_path(root.path(), "workspace-safe", oversized_thread);
        let oversized = File::create(&oversized_path).unwrap();
        oversized.set_len(MAX_FILE_BYTES + 1).unwrap();
        assert!(store.read(oversized_thread).is_err());

        let huge = "x".repeat(MAX_RECORD_BYTES + 1);
        assert!(store.sync(thread_id, 1, &[entry(2, &huge)]).is_err());

        let empty_thread = ThreadId::from_uuid(ids.next_uuid_v7());
        assert!(store.sync(empty_thread, 1, &[]).is_err());
        assert!(io_error(std::io::Error::other("io")).contains("conversation storage failure"));
        let json = serde_json::from_str::<Value>("{").unwrap_err();
        assert!(json_error(json).contains("invalid conversation JSONL"));
    }

    #[test]
    fn rejects_identity_sequence_and_record_corruption() {
        let root = tempfile::tempdir().unwrap();
        let workspace_key = "workspace-errors";
        let store = ConversationStore::open(root.path(), workspace_key).unwrap();
        let ids = SystemIdSource::default();
        let thread_id = ThreadId::from_uuid(ids.next_uuid_v7());
        let first = entry(1, "first");
        store
            .sync(thread_id, 1, std::slice::from_ref(&first))
            .unwrap();

        let collision = entry(1, "collision");
        assert!(store.sync(thread_id, 1, &[collision]).is_err());
        assert!(store.sync(thread_id, 1, &[entry(0, "zero")]).is_err());

        let path = session_path(root.path(), workspace_key, thread_id);
        let another_thread = ThreadId::from_uuid(ids.next_uuid_v7());
        let another_path = session_path(root.path(), workspace_key, another_thread);
        fs::copy(&path, &another_path).unwrap();
        assert!(store.read(another_thread).is_err());

        let malformed_thread = ThreadId::from_uuid(ids.next_uuid_v7());
        let malformed_path = session_path(root.path(), workspace_key, malformed_thread);
        let valid_header = header(malformed_thread, workspace_key);
        let valid_record = record(&first);
        let mut invalid_type = valid_record.clone();
        invalid_type["record"] = json!("unknown");
        overwrite_records(&malformed_path, &valid_header, &[invalid_type]);
        assert!(store.read(malformed_thread).is_err());

        let mut missing_sequence = valid_record.clone();
        missing_sequence.as_object_mut().unwrap().remove("seq");
        overwrite_records(&malformed_path, &valid_header, &[missing_sequence]);
        assert!(store.read(malformed_thread).is_err());

        let mut missing_entry_id = valid_record.clone();
        missing_entry_id.as_object_mut().unwrap().remove("entry_id");
        overwrite_records(&malformed_path, &valid_header, &[missing_entry_id]);
        assert!(store.read(malformed_thread).is_err());

        let mut invalid_entry_id = valid_record.clone();
        invalid_entry_id["entry_id"] = json!("not-a-uuid");
        overwrite_records(&malformed_path, &valid_header, &[invalid_entry_id]);
        assert!(store.read(malformed_thread).is_err());

        let mut invalid_run_id = valid_record.clone();
        invalid_run_id["run_id"] = json!(42);
        overwrite_records(&malformed_path, &valid_header, &[invalid_run_id]);
        assert!(store.read(malformed_thread).is_err());

        let mut missing_kind = valid_record.clone();
        missing_kind.as_object_mut().unwrap().remove("kind");
        overwrite_records(&malformed_path, &valid_header, &[missing_kind]);
        assert!(store.read(malformed_thread).is_err());

        let mut missing_content = valid_record.clone();
        missing_content.as_object_mut().unwrap().remove("content");
        overwrite_records(&malformed_path, &valid_header, &[missing_content]);
        assert!(store.read(malformed_thread).is_err());

        let mut missing_source = valid_record.clone();
        missing_source.as_object_mut().unwrap().remove("source_key");
        overwrite_records(&malformed_path, &valid_header, &[missing_source]);
        assert!(store.read(malformed_thread).is_err());

        let mut missing_created_at = valid_record;
        missing_created_at
            .as_object_mut()
            .unwrap()
            .remove("created_at_ms");
        overwrite_records(&malformed_path, &valid_header, &[missing_created_at]);
        assert!(store.read(malformed_thread).is_err());

        let mut duplicate = record(&first);
        duplicate["entry_id"] = json!(TranscriptEntryId::from_uuid(ids.next_uuid_v7()));
        overwrite_records(&malformed_path, &valid_header, &[record(&first), duplicate]);
        assert!(store.read(malformed_thread).is_err());

        let oversized_line = Value::String("x".repeat(MAX_RECORD_BYTES + 1));
        overwrite_records(&malformed_path, &valid_header, &[oversized_line]);
        assert!(store.read(malformed_thread).is_err());
    }

    #[test]
    fn repair_rejects_file_with_no_newline() {
        let root = tempfile::tempdir().unwrap();
        let store = ConversationStore::open(root.path(), "workspace-torn").unwrap();
        let thread_id = ThreadId::from_uuid(SystemIdSource::default().next_uuid_v7());
        // Write a file with content but no newline at all — the header is torn.
        let path = session_path(root.path(), "workspace-torn", thread_id);
        fs::write(&path, b"{\"record\":\"header\"").unwrap();
        // read should fail because the header cannot be parsed.
        assert!(store.read(thread_id).is_err());
    }
}
