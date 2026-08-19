use super::support::Scenario;
use latte_core::{RunId, RunStatus, Transition, VerificationStatus};
use latte_engine::{
    CancellationToken, EffectStatus, EngineHandle, Lease, ProcessError, ProcessInvocation,
    ProcessTermination,
};
use std::{collections::BTreeMap, time::SystemTime};

fn wall_now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn fixture_engine(scenario: &Scenario) -> EngineHandle {
    std::fs::create_dir_all(scenario.database_path().parent().unwrap()).unwrap();
    latte_engine::EngineBuilder::new()
        .workspace_root(scenario.root())
        .database_path(scenario.database_path())
        .build()
        .unwrap()
}

fn invocation<'a>(
    argv: &'a [String],
    shell: Option<&'a str>,
    effect_id: &'a str,
    env: &'a BTreeMap<String, String>,
    lease: &'a Lease,
) -> ProcessInvocation<'a> {
    ProcessInvocation {
        argv,
        shell,
        cwd: ".",
        env,
        timeout_ms: 2_000,
        grace_ms: 50,
        stdout_cap: 1_024,
        stderr_cap: 1_024,
        run_revision: 0,
        effect_id,
        attempt: 1,
        approval_digest: None,
        lease_owner: lease.owner(),
        lease_token: lease.fencing_token(),
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn public_process_safety_and_authority_matrix_is_durable_in_final_cli() {
    let scenario = Scenario::new();
    std::fs::write(scenario.root().join("not-a-directory"), "sentinel").unwrap();
    let engine = fixture_engine(&scenario);
    let now = wall_now_ms();
    let run_id = RunId::from_uuid(uuid::Uuid::now_v7());
    engine.create_run(run_id, now).unwrap();
    let lease = engine
        .acquire_lease("process-boundary", now + 1, 120_000)
        .unwrap();
    let empty = Vec::new();
    let pwd = vec!["/bin/pwd".to_owned()];
    let env = BTreeMap::new();

    let both = invocation(&pwd, Some("pwd"), "invalid-both", &env, &lease);
    assert!(matches!(
        engine
            .execute_process(run_id, &lease, now + 2, &both, &CancellationToken::new())
            .await,
        Err(ProcessError::Invalid(message)) if message.contains("exactly one")
    ));
    let neither = invocation(&empty, None, "invalid-neither", &env, &lease);
    assert!(matches!(
        engine
            .execute_process(run_id, &lease, now + 3, &neither, &CancellationToken::new())
            .await,
        Err(ProcessError::Invalid(message)) if message.contains("exactly one")
    ));
    let zero_cap = ProcessInvocation {
        stdout_cap: 0,
        ..invocation(&pwd, None, "invalid-zero-cap", &env, &lease)
    };
    assert!(matches!(
        engine
            .execute_process(
                run_id,
                &lease,
                now + 4,
                &zero_cap,
                &CancellationToken::new(),
            )
            .await,
        Err(ProcessError::Invalid(message)) if message.contains("nonzero")
    ));
    let wrong_owner = ProcessInvocation {
        lease_owner: "foreign-owner",
        ..invocation(&pwd, None, "invalid-owner", &env, &lease)
    };
    assert!(matches!(
        engine
            .execute_process(
                run_id,
                &lease,
                now + 5,
                &wrong_owner,
                &CancellationToken::new(),
            )
            .await,
        Err(ProcessError::InvalidApproval)
    ));
    let file_cwd = ProcessInvocation {
        cwd: "not-a-directory",
        ..invocation(&pwd, None, "invalid-cwd", &env, &lease)
    };
    assert!(matches!(
        engine
            .execute_process(
                run_id,
                &lease,
                now + 6,
                &file_cwd,
                &CancellationToken::new(),
            )
            .await,
        Err(ProcessError::Invalid(message)) if message.contains("must be a directory")
    ));
    let dangerous = invocation(
        &empty,
        Some("mkfs /dev/example"),
        "denied-shell",
        &env,
        &lease,
    );
    assert!(matches!(
        engine
            .execute_process(
                run_id,
                &lease,
                now + 7,
                &dangerous,
                &CancellationToken::new(),
            )
            .await,
        Err(ProcessError::Denied)
    ));
    let wrong_digest = ProcessInvocation {
        approval_digest: Some("not-an-issued-digest"),
        ..invocation(
            &empty,
            Some("printf safe"),
            "unissued-approval",
            &env,
            &lease,
        )
    };
    assert!(matches!(
        engine
            .execute_process(
                run_id,
                &lease,
                now + 8,
                &wrong_digest,
                &CancellationToken::new(),
            )
            .await,
        Err(ProcessError::InvalidApproval)
    ));
    for effect_id in [
        "invalid-both",
        "invalid-neither",
        "invalid-zero-cap",
        "invalid-owner",
        "invalid-cwd",
        "denied-shell",
        "unissued-approval",
    ] {
        assert!(
            engine.effect_status(effect_id).is_err(),
            "rejected request {effect_id} created durable authority"
        );
    }
    let unchanged = engine.show(run_id).unwrap();
    assert_eq!(unchanged.revision, 0);
    assert_eq!(unchanged.status, RunStatus::Queued);
    assert_eq!(
        std::fs::read_to_string(scenario.root().join("not-a-directory")).unwrap(),
        "sentinel"
    );

    let noisy_shell = "printf '0123456789abcdef'; printf 'fedcba9876543210' >&2";
    let prepared = ProcessInvocation {
        stdout_cap: 8,
        stderr_cap: 8,
        ..invocation(&empty, Some(noisy_shell), "prepared-process", &env, &lease)
    };
    let prepared_digest = match engine
        .execute_process(
            run_id,
            &lease,
            now + 9,
            &prepared,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err()
    {
        ProcessError::PermissionRequired { digest } => digest,
        error => panic!("unexpected preparation error: {error}"),
    };
    assert_eq!(
        engine.effect_status("prepared-process").unwrap(),
        EffectStatus::Prepared
    );
    let allow_reissue = invocation(&pwd, None, "allow-reissue", &env, &lease);
    assert!(matches!(
        engine.reissue_process_permission(
            "prepared-process",
            run_id,
            &lease,
            now + 10,
            &allow_reissue,
        ),
        Err(ProcessError::Invalid(message)) if message.contains("only ask")
    ));
    let approved = ProcessInvocation {
        approval_digest: Some(&prepared_digest),
        ..prepared
    };
    let output = engine
        .execute_process(
            run_id,
            &lease,
            now + 11,
            &approved,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output.termination, ProcessTermination::Exited);
    assert_eq!(output.stdout, "01234567");
    assert_eq!(output.stderr, "fedcba98");
    assert!(output.stdout_truncated && output.stderr_truncated);
    assert_eq!(
        engine.effect_status("prepared-process").unwrap(),
        EffectStatus::ObservedSuccess
    );

    let running = engine
        .apply_transition(run_id, 0, Transition::Start, now + 12, &lease)
        .unwrap();
    let verification = ProcessInvocation {
        run_revision: running.revision,
        effect_id: "final-verification",
        ..invocation(&pwd, None, "unused", &env, &lease)
    };
    let verification_output = engine
        .execute_verification(
            run_id,
            running.revision,
            &lease,
            now + 13,
            &verification,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(verification_output.command_succeeded());
    engine
        .complete_verified_run(
            run_id,
            running.revision,
            &lease,
            "process boundary verified".into(),
            now + 14,
        )
        .unwrap();
    let completed = engine.show(run_id).unwrap();
    assert_eq!(completed.status, RunStatus::Completed);
    let handoff = completed.handoff.expect("completed run must have handoff");
    assert_eq!(handoff.summary, "process boundary verified");
    assert_eq!(handoff.evidence[0].status, VerificationStatus::Passed);
    engine.release_lease(&lease).unwrap();
}
