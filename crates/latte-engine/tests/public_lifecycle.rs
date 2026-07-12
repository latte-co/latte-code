use latte_core::{
    CommandId, EventId, IdSource, RunId, RunStatus, RuntimeEvent, SystemIdSource, Transition,
};
use latte_engine::{EffectStatus, EngineBuilder, StorageError, ToolError, ToolInvocation};
use serde_json::json;

fn ids() -> (RunId, EventId) {
    let source = SystemIdSource::default();
    (
        RunId::from_uuid(source.next_uuid_v7()),
        EventId::from_uuid(source.next_uuid_v7()),
    )
}

#[test]
fn typed_ids_expose_their_underlying_uuid_without_string_roundtrips() {
    let source = SystemIdSource::default();
    let run = RunId::from_uuid(source.next_uuid_v7());
    let command = CommandId::from_uuid(source.next_uuid_v7());
    let event = EventId::from_uuid(source.next_uuid_v7());
    assert_eq!(RunId::from_uuid(run.as_uuid()), run);
    assert_eq!(CommandId::from_uuid(command.as_uuid()), command);
    assert_eq!(EventId::from_uuid(event.as_uuid()), event);
}

#[test]
fn expired_pending_tool_permission_is_reissued_to_a_fresh_fenced_lease() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "old").unwrap();
    let engine = EngineBuilder::new()
        .workspace_root(dir.path())
        .build()
        .unwrap();
    let run_id = RunId::from_uuid(SystemIdSource::default().next_uuid_v7());
    engine.create_run(run_id, 1).unwrap();
    let old_lease = engine.acquire_lease("old-owner", 2, 5).unwrap();
    let read_input = json!({"path":"a.txt"});
    let read = ToolInvocation {
        name: "read_file",
        input: &read_input,
        run_revision: 0,
        effect_id: "read-reissue",
        attempt: 1,
        precondition: None,
        timeout_ms: 0,
        output_cap: 1024,
        approval_digest: None,
        lease_owner: old_lease.owner(),
        lease_token: old_lease.fencing_token(),
    };
    let hash = engine
        .execute_tool(run_id, &old_lease, 3, &read)
        .unwrap()
        .value["sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    let write_input = json!({"path":"a.txt","content":"new"});
    let old_write = ToolInvocation {
        name: "write_file",
        input: &write_input,
        run_revision: 0,
        effect_id: "old-write",
        attempt: 1,
        precondition: Some(&hash),
        timeout_ms: 0,
        output_cap: 1024,
        approval_digest: None,
        lease_owner: old_lease.owner(),
        lease_token: old_lease.fencing_token(),
    };
    assert!(matches!(
        engine.execute_tool(run_id, &old_lease, 4, &old_write),
        Err(ToolError::PermissionRequired { .. })
    ));

    let fresh = engine.acquire_lease("fresh-owner", 8, 100).unwrap();
    let fresh_write = ToolInvocation {
        effect_id: "fresh-write",
        attempt: 2,
        lease_owner: fresh.owner(),
        lease_token: fresh.fencing_token(),
        ..old_write
    };
    let digest = engine
        .reissue_tool_permission("old-write", run_id, &fresh, 9, &fresh_write)
        .unwrap();
    assert!(!digest.is_empty());
    // The fresh binding is still fail-closed until all exact resume inputs match.
    assert!(
        !engine
            .permission_matches("fresh-write", run_id, 0, &fresh, "wrong-digest", 10)
            .unwrap()
    );
    assert_eq!(
        engine.effect_status("old-write").unwrap(),
        EffectStatus::ObservedFailed
    );
    assert_eq!(
        engine.effect_status("fresh-write").unwrap(),
        EffectStatus::Prepared
    );
}

#[tokio::test]
async fn public_storage_effect_event_and_subscription_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let engine = EngineBuilder::new()
        .workspace_root(dir.path())
        .database_path(dir.path().join("state.db"))
        .enabled_tools(["read_file", "list_files"])
        .deny_globs(["secret/**"])
        .build()
        .unwrap();
    assert_eq!(engine.tool_descriptors().len(), 2);
    assert!(engine.changed_files().is_err());
    assert!(
        !engine
            .workspace_manifest()
            .unwrap()
            .contains_key(r#"["state.db"]"#)
    );
    std::fs::write(dir.path().join("state.db-important.rs"), "one").unwrap();
    let first_manifest = engine.workspace_manifest().unwrap();
    let important_key = r#"["state.db-important.rs"]"#;
    assert!(first_manifest.contains_key(important_key));
    std::fs::write(dir.path().join("state.db-important.rs"), "two").unwrap();
    let second_manifest = engine.workspace_manifest().unwrap();
    assert_ne!(
        first_manifest[important_key],
        second_manifest[important_key]
    );

    let (run_id, _event_id) = ids();
    let queued = engine.create_run(run_id, 1).unwrap();
    assert_eq!(engine.show(run_id).unwrap(), queued);
    assert_eq!(engine.list().unwrap(), vec![queued.clone()]);
    let lease = engine.acquire_lease("integration", 2, 100).unwrap();
    let renewed = engine.renew_lease(&lease, 3, 100).unwrap();
    let mut subscription = engine.subscribe();
    assert!(subscription.try_recv().unwrap().is_none());
    let running = engine
        .apply_transition(run_id, 0, Transition::Start, 4, &renewed)
        .unwrap();
    let received = subscription.recv().await.unwrap();
    assert_eq!(received.run_id, run_id);
    assert_eq!(received.revision, 1);
    assert_eq!(
        received.event,
        RuntimeEvent::StateChanged {
            status: RunStatus::Running
        }
    );
    assert!(matches!(
        engine.apply_transition(run_id, 0, Transition::Start, 5, &renewed),
        Err(StorageError::StaleRevision { .. })
    ));

    engine
        .persist_runtime_checkpoint(run_id, running.revision, &renewed, r#"{"step":1}"#, 12)
        .unwrap();
    assert_eq!(
        engine.runtime_checkpoint(run_id).unwrap().as_deref(),
        Some(r#"{"step":1}"#)
    );
    engine.release_lease(&renewed).unwrap();
    assert!(matches!(
        engine.renew_lease(&renewed, 13, 10),
        Err(StorageError::LeaseLost)
    ));
}

#[test]
fn config_loader_and_builder_validation_are_exercised_through_public_api() {
    let dir = tempfile::tempdir().unwrap();
    let missing = latte_engine::config::Config::load(dir.path()).unwrap_err();
    assert!(missing.to_string().contains("cannot read configuration"));

    let config_dir = dir.path().join(".latte");
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("lattecode.jsonc"),
        r#"{
          database: { path: ".latte/test.db" },
          runtime: { command_buffer: 4, event_buffer: 8 },
          providers: { primary: { base_url: "http://localhost", api_key: "secret" } }
        }"#,
    )
    .unwrap();
    let config = latte_engine::config::Config::load(dir.path()).unwrap();
    assert_eq!(config.runtime.command_buffer, 4);
    assert!(format!("{config:?}").contains("[REDACTED]"));

    std::fs::write(
        config_dir.join("missing.jsonc"),
        r#"{ providers: { p: { base_url: "http://localhost", api_key: "${LATTE_TEST_DEFINITELY_MISSING}" } } }"#,
    )
    .unwrap();
    let missing_environment =
        latte_engine::config::Config::load_path(&config_dir.join("missing.jsonc")).unwrap_err();
    assert!(
        missing_environment
            .to_string()
            .contains("missing environment")
    );

    for (name, body, expected) in [
        (
            "empty-db.jsonc",
            r#"{ database: { path: "" } }"#,
            "database.path must not be empty",
        ),
        (
            "empty-provider.jsonc",
            r#"{ providers: { p: { base_url: "", api_key: "key" } } }"#,
            "requires base_url and api_key",
        ),
        (
            "bad-placeholder.jsonc",
            r#"{ database: { path: "${}" } }"#,
            "invalid environment placeholder",
        ),
    ] {
        let path = config_dir.join(name);
        std::fs::write(&path, body).unwrap();
        let error = latte_engine::config::Config::load_path(&path).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
    let environment_path = config_dir.join("environment.jsonc");
    std::fs::write(
        &environment_path,
        r#"{ providers: { p: { base_url: "${PATH}", api_key: "key" } } }"#,
    )
    .unwrap();
    assert!(
        !latte_engine::config::Config::load_path(&environment_path)
            .unwrap()
            .providers["p"]
            .base_url
            .is_empty()
    );

    let default_engine = EngineBuilder::new().build().unwrap();
    assert!(!default_engine.tool_descriptors().is_empty());

    let invalid = EngineBuilder::new()
        .workspace_root(dir.path().join("absent"))
        .build()
        .unwrap_err();
    assert!(matches!(invalid, StorageError::InvalidData(_)));
}
