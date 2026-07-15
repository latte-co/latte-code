use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Command, Output, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub struct Scenario {
    root: tempfile::TempDir,
}

impl Scenario {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        Self { root }
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn home(&self) -> std::path::PathBuf {
        self.root.path().join("home")
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_latte-code"));
        command
            .args(args)
            .current_dir(self.root.path())
            .env("HOME", self.home())
            .env_remove("LATTE_OPENAI_ENDPOINT")
            .env_remove("LATTE_OPENAI_MODEL")
            .env_remove("LATTE_OPENAI_API_KEY")
            .env_remove("LATTE_VERIFY_ARGV")
            .env_remove("OPENAI_API_KEY")
            .env_remove("TEST_OPENAI_KEY")
            .stdin(Stdio::null());
        preserve_coverage_profiles(&mut command);
        command
    }

    pub fn output(&self, args: &[&str], configure: impl FnOnce(&mut Command)) -> Output {
        let mut command = self.command(args);
        configure(&mut command);
        bounded_output(command, Duration::from_secs(10))
    }

    pub fn write_config(&self, endpoint: &str, verification: &str) {
        self.write_config_with_database(endpoint, verification, ".latte/latte-code.db");
    }

    pub fn write_config_with_database(
        &self,
        endpoint: &str,
        verification: &str,
        database_path: &str,
    ) {
        self.write_config_with_provider_fields(endpoint, verification, database_path, "");
    }

    pub fn write_config_with_base_url(&self, base_url: &str, verification: &str) {
        std::fs::create_dir_all(self.root.path().join(".latte")).unwrap();
        std::fs::write(
            self.root.path().join(".latte/latte-code.jsonc"),
            format!(
                r#"{{version:1,default_provider:"main",providers:{{main:{{type:"openai-chat",model:"mock",base_url:{base_url:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}},credential_ref_id:"env:TEST_OPENAI_KEY",data_scope_id:"workspace",credential_generation:1}}}},database:{{path:".latte/latte-code.db"}},verification:{{argv:{verification}}}}}"#
            ),
        )
        .unwrap();
    }

    pub fn write_config_with_provider_fields(
        &self,
        endpoint: &str,
        verification: &str,
        database_path: &str,
        provider_fields: &str,
    ) {
        std::fs::create_dir_all(self.root.path().join(".latte")).unwrap();
        std::fs::write(
            self.root.path().join(".latte/latte-code.jsonc"),
            format!(
                r#"{{version:1,default_provider:"main",providers:{{main:{{type:"openai-chat",model:"mock",endpoint:{endpoint:?},api_key:{{source:"env",name:"TEST_OPENAI_KEY"}},credential_ref_id:"env:TEST_OPENAI_KEY",data_scope_id:"workspace",credential_generation:1{provider_fields}}}}},database:{{path:{database_path:?}}},verification:{{argv:{verification}}}}}"#
            ),
        )
        .unwrap();
    }

    pub fn init_git(&self) {
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(self.root())
            .env("HOME", self.home())
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .args(["config", "user.email", "e2e@example.invalid"])
            .current_dir(self.root())
            .env("HOME", self.home())
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .args(["config", "user.name", "Latte E2E"])
            .current_dir(self.root())
            .env("HOME", self.home())
            .status()
            .unwrap();
        assert!(status.success());
    }

    pub fn configure_provider(
        &self,
        command: &mut Command,
        endpoint: &str,
        verification: &str,
        secret: &str,
    ) {
        self.write_config(endpoint, verification);
        command.env("TEST_OPENAI_KEY", secret);
    }

    pub fn database_path(&self) -> std::path::PathBuf {
        self.root.path().join(".latte/latte-code.db")
    }
}

pub fn isolated_output(args: &[&str], configure: impl FnOnce(&Scenario, &mut Command)) -> Output {
    let scenario = Scenario::new();
    scenario.output(args, |command| configure(&scenario, command))
}

pub fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

pub fn preserve_coverage_profiles(command: &mut Command) {
    if let Ok(profile) = std::env::var("LLVM_PROFILE_FILE") {
        command.env(
            "LLVM_PROFILE_FILE",
            profile.replace(".profraw", "-%p.profraw"),
        );
    }
}

fn bounded_output(mut command: Command, timeout: Duration) -> Output {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let process_group = i32::try_from(child.id()).unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::BufReader::new(stdout)
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::BufReader::new(stderr)
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            {
                let group = nix::unistd::Pid::from_raw(process_group);
                let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            let stdout = stdout_reader.join().unwrap();
            let stderr = stderr_reader.join().unwrap();
            panic!(
                "final binary exceeded {timeout:?}; stdout={} stderr={}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: stdout_reader.join().unwrap(),
        stderr: stderr_reader.join().unwrap(),
    }
}

pub fn assert_secret_absent(secret: &str, surfaces: &[(&str, &[u8])]) {
    for (name, bytes) in surfaces {
        assert!(
            !bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "secret sentinel was present in {name}"
        );
    }
}

#[derive(Clone, Debug)]
pub struct ProviderRequest {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Value,
}

#[derive(Clone, Debug)]
pub struct ProviderReply {
    status: u16,
    content_type: String,
    body: Vec<u8>,
    headers: BTreeMap<String, String>,
    delay_before_reply: Duration,
    chunk_size: Option<usize>,
    inter_chunk_delay: Duration,
}

impl ProviderReply {
    pub fn completion(content: &str) -> Self {
        Self::json(
            200,
            &serde_json::json!({
                "choices": [{"message": {"content": content, "tool_calls": []}}]
            }),
        )
    }

    pub fn tool_call(id: &str, name: &str, arguments: &Value) -> Self {
        Self::tool_calls([(id, name, arguments)])
    }

    pub fn tool_calls<'a>(calls: impl IntoIterator<Item = (&'a str, &'a str, &'a Value)>) -> Self {
        let calls = calls
            .into_iter()
            .map(|(id, name, arguments)| {
                serde_json::json!({
                    "id": id,
                    "function": {
                        "name": name,
                        "arguments": serde_json::to_string(arguments).unwrap()
                    }
                })
            })
            .collect::<Vec<_>>();
        Self::json(
            200,
            &serde_json::json!({
                "choices": [{
                    "message": {
                        "content": "",
                        "tool_calls": calls
                    }
                }]
            }),
        )
    }

    pub fn input_request(id: &str, prompt: &str, secret: bool) -> Self {
        Self::json(
            200,
            &serde_json::json!({
                "choices": [{
                    "message": {
                        "content": null,
                        "tool_calls": [],
                        "input_request": {"id": id, "prompt": prompt, "secret": secret}
                    },
                    "finish_reason": "stop"
                }]
            }),
        )
    }

    pub fn json(status: u16, body: &Value) -> Self {
        Self::raw(
            status,
            "application/json",
            serde_json::to_vec(&body).unwrap(),
        )
    }

    pub fn raw(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: content_type.into(),
            body: body.into(),
            headers: BTreeMap::new(),
            delay_before_reply: Duration::ZERO,
            chunk_size: None,
            inter_chunk_delay: Duration::ZERO,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay_before_reply = delay;
        self
    }

    pub fn chunked(mut self, chunk_size: usize, inter_chunk_delay: Duration) -> Self {
        assert!(chunk_size > 0);
        self.chunk_size = Some(chunk_size);
        self.inter_chunk_delay = inter_chunk_delay;
        self
    }
}

#[derive(Default)]
struct ProviderState {
    replies: VecDeque<ProviderReply>,
    requests: Vec<ProviderRequest>,
    errors: Vec<String>,
}

struct SharedProviderState {
    state: Mutex<ProviderState>,
    changed: Condvar,
}

pub struct ScriptedProvider {
    endpoint: String,
    shared: Arc<SharedProviderState>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ScriptedProvider {
    pub fn start(replies: impl IntoIterator<Item = ProviderReply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shared = Arc::new(SharedProviderState {
            state: Mutex::new(ProviderState {
                replies: replies.into_iter().collect(),
                ..ProviderState::default()
            }),
            changed: Condvar::new(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let worker_shared = Arc::clone(&shared);
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                if worker_stop.load(Ordering::Acquire) {
                    break;
                }
                let request = read_provider_request(&mut stream);
                let reply = {
                    let mut state = worker_shared
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match request {
                        Ok(request) => state.requests.push(request),
                        Err(error) => state.errors.push(error),
                    }
                    let reply = state.replies.pop_front();
                    if reply.is_none() {
                        state
                            .errors
                            .push("unexpected extra provider request".into());
                    }
                    worker_shared.changed.notify_all();
                    reply.unwrap_or_else(|| ProviderReply {
                        status: 500,
                        content_type: "application/json".into(),
                        body: br#"{"error":"unexpected request"}"#.to_vec(),
                        headers: BTreeMap::new(),
                        delay_before_reply: Duration::ZERO,
                        chunk_size: None,
                        inter_chunk_delay: Duration::ZERO,
                    })
                };
                if let Err(error) = write_provider_reply(&mut stream, &reply) {
                    let mut state = worker_shared
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.errors.push(error);
                    worker_shared.changed.notify_all();
                }
            }
        });
        Self {
            endpoint: format!("http://{address}/chat/completions"),
            shared,
            stop,
            worker: Some(worker),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn wait_for_calls(&self, count: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.requests.len() < count && state.errors.is_empty() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let waited = self.shared.changed.wait_timeout(state, remaining).unwrap();
            state = waited.0;
            if waited.1.timed_out() {
                break;
            }
        }
        state.requests.len() >= count
    }

    pub fn requests(&self) -> Vec<ProviderRequest> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .requests
            .clone()
    }

    pub fn assert_consumed(&self) {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            state.errors.is_empty(),
            "provider errors: {:?}",
            state.errors
        );
        assert!(
            state.replies.is_empty(),
            "{} scripted provider replies were not consumed",
            state.replies.len()
        );
    }
}

impl Drop for ScriptedProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(
            self.endpoint
                .trim_start_matches("http://")
                .trim_end_matches("/chat/completions"),
        );
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_provider_request(stream: &mut TcpStream) -> Result<ProviderRequest, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    let (header_end, content_length) = loop {
        let count = stream
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("provider request ended before its complete body".into());
        }
        bytes.extend_from_slice(&buffer[..count]);
        let Some(split) = bytes.windows(4).position(|value| value == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..split]);
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .ok_or_else(|| "provider request omitted Content-Length".to_owned())?;
        if bytes.len() >= split + 4 + length {
            break (split, length);
        }
    };
    let header_text =
        std::str::from_utf8(&bytes[..header_end]).map_err(|error| error.to_string())?;
    let mut lines = header_text.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| "provider request omitted request line".to_owned())?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_owned();
    let path = request_parts.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let body_start = header_end + 4;
    let body = serde_json::from_slice(&bytes[body_start..body_start + content_length])
        .map_err(|error| error.to_string())?;
    Ok(ProviderRequest {
        method,
        path,
        headers,
        body,
    })
}

fn write_provider_reply(stream: &mut TcpStream, reply: &ProviderReply) -> Result<(), String> {
    if !reply.delay_before_reply.is_zero() {
        thread::sleep(reply.delay_before_reply);
    }
    let reason = match reply.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        408 => "Request Timeout",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Response",
    };
    write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    )
    .map_err(|error| error.to_string())?;
    for (name, value) in &reply.headers {
        write!(stream, "{name}: {value}\r\n").map_err(|error| error.to_string())?;
    }
    stream
        .write_all(b"\r\n")
        .map_err(|error| error.to_string())?;
    if let Some(chunk_size) = reply.chunk_size {
        for (index, chunk) in reply.body.chunks(chunk_size).enumerate() {
            stream
                .write_all(chunk)
                .and_then(|()| stream.flush())
                .map_err(|error| error.to_string())?;
            if (index + 1) * chunk_size < reply.body.len() && !reply.inter_chunk_delay.is_zero() {
                thread::sleep(reply.inter_chunk_delay);
            }
        }
        Ok(())
    } else {
        stream
            .write_all(&reply.body)
            .map_err(|error| error.to_string())
    }
}

#[cfg(unix)]
struct Capture {
    bytes: Mutex<Vec<u8>>,
    changed: Condvar,
}

#[cfg(unix)]
pub struct PtySession {
    input: Option<std::fs::File>,
    output: Arc<Capture>,
    child: Option<std::process::Child>,
    reader: Option<thread::JoinHandle<()>>,
    process_group: i32,
}

#[cfg(unix)]
impl PtySession {
    pub fn spawn(command: Command) -> Self {
        Self::spawn_with_size(command, 40, 120)
    }

    pub fn spawn_with_size(mut command: Command, rows: u16, columns: u16) -> Self {
        use std::os::unix::process::CommandExt;

        let pty = nix::pty::openpty(
            Some(&nix::pty::Winsize {
                ws_row: rows,
                ws_col: columns,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None::<&nix::sys::termios::Termios>,
        )
        .unwrap();
        let master = std::fs::File::from(pty.master);
        let input = master.try_clone().unwrap();
        let slave_stdout = pty.slave.try_clone().unwrap();
        let slave_stderr = pty.slave.try_clone().unwrap();
        command
            .stdin(Stdio::from(pty.slave))
            .stdout(Stdio::from(slave_stdout))
            .stderr(Stdio::from(slave_stderr));
        command.process_group(0);
        let child = command.spawn().unwrap();
        let process_group = i32::try_from(child.id()).unwrap();
        let output = Arc::new(Capture {
            bytes: Mutex::new(Vec::new()),
            changed: Condvar::new(),
        });
        let reader_output = Arc::clone(&output);
        let reader = thread::spawn(move || {
            let mut stream = master;
            let mut buffer = [0_u8; 4096];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        reader_output
                            .bytes
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .extend_from_slice(&buffer[..count]);
                        reader_output.changed.notify_all();
                    }
                }
            }
            reader_output.changed.notify_all();
        });
        Self {
            input: Some(input),
            output,
            child: Some(child),
            reader: Some(reader),
            process_group,
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.input.as_mut().unwrap().write_all(bytes).unwrap();
    }

    pub fn resize(&mut self, rows: u16, columns: u16) {
        let status = Command::new("stty")
            .args(["rows", &rows.to_string(), "cols", &columns.to_string()])
            .stdin(Stdio::from(
                self.input.as_ref().unwrap().try_clone().unwrap(),
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "failed to resize test PTY");
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.process_group),
            nix::sys::signal::Signal::SIGWINCH,
        )
        .unwrap();
    }

    pub fn wait_for_output(&self, needle: &[u8], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut output = self
            .output
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if output.windows(needle.len()).any(|window| window == needle) {
                return true;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let waited = self.output.changed.wait_timeout(output, remaining).unwrap();
            output = waited.0;
            if waited.1.timed_out() {
                return output.windows(needle.len()).any(|window| window == needle);
            }
        }
    }

    pub fn output(&self) -> Vec<u8> {
        self.output
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn wait_for_growth(&self, previous_len: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut output = self
            .output
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while output.len() <= previous_len {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let waited = self.output.changed.wait_timeout(output, remaining).unwrap();
            output = waited.0;
            if waited.1.timed_out() {
                return output.len() > previous_len;
            }
        }
        true
    }

    pub fn is_running(&mut self) -> bool {
        self.child.as_mut().unwrap().try_wait().unwrap().is_none()
    }

    pub fn finish(mut self, timeout: Duration) -> (std::process::ExitStatus, Vec<u8>) {
        self.input.take();
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let group = nix::unistd::Pid::from_raw(self.process_group);
                let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
                let child = self.child.as_mut().unwrap();
                let _ = child.kill();
                let _ = child.wait();
                self.child.take();
                if let Some(reader) = self.reader.take() {
                    let _ = reader.join();
                }
                panic!(
                    "PTY child exceeded {timeout:?}: {}",
                    String::from_utf8_lossy(&self.output())
                );
            }
            thread::sleep(Duration::from_millis(10));
        };
        self.child.take();
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
        (status, self.output())
    }
}

#[cfg(unix)]
impl Drop for PtySession {
    fn drop(&mut self) {
        self.input.take();
        if let Some(mut child) = self.child.take()
            && child.try_wait().ok().flatten().is_none()
        {
            let group = nix::unistd::Pid::from_raw(self.process_group);
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

pub fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    predicate()
}

#[cfg(unix)]
pub fn assert_process_group_gone(pgid: i32, timeout: Duration) {
    let gone = wait_until(timeout, || {
        matches!(
            nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), None),
            Err(nix::errno::Errno::ESRCH)
        )
    });
    if !gone {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    assert!(gone, "process group {pgid} survived its terminal boundary");
}
