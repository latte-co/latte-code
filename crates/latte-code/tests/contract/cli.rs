#[test]
fn headless_parser_and_placeholder_cover_every_public_command_shape() {
    use latte_headless::{HeadlessCommand, parse, render_placeholder};
    let root = tempfile::tempdir().unwrap();
    let engine = latte_engine::EngineBuilder::new()
        .workspace_root(root.path())
        .build()
        .unwrap();
    let run_id = latte_core::RunId::from_uuid(
        uuid::Uuid::parse_str("01900000-0000-7000-8000-000000000001").unwrap(),
    );
    let commands = [
        HeadlessCommand::List,
        HeadlessCommand::Run {
            prompt: "work".into(),
            focus: None,
        },
        HeadlessCommand::Resume {
            run_id,
            allow: true,
        },
        HeadlessCommand::Show { run_id },
    ];
    for command in &commands {
        assert!(!render_placeholder(command, &engine).is_empty());
    }
    assert!(parse(&["run".into()]).is_err());
    assert!(parse(&["run".into(), "--focus".into()]).is_err());
    assert!(parse(&["run".into(), "x".into(), "--focus".into()]).is_err());
}
