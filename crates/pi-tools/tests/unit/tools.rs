use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A fresh temp directory per test.
fn temp_ctx() -> ToolContext {
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("pi-tools-test-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    ToolContext::new(dir)
}

#[test]
fn read_returns_file_contents() {
    let ctx = temp_ctx();
    std::fs::write(ctx.cwd.join("a.txt"), "hello\nworld\n").expect("write");
    let result = ReadTool.run(&serde_json::json!({ "path": "a.txt" }), &ctx);
    assert!(!result.is_error);
    assert_eq!(result.content, "hello\nworld\n");
}

#[test]
fn read_missing_file_is_an_error() {
    let ctx = temp_ctx();
    let result = ReadTool.run(&serde_json::json!({ "path": "nope.txt" }), &ctx);
    assert!(result.is_error);
}

#[test]
fn read_honors_offset_and_limit() {
    let ctx = temp_ctx();
    std::fs::write(ctx.cwd.join("a.txt"), "1\n2\n3\n4\n5\n").expect("write");
    let result = ReadTool.run(
        &serde_json::json!({ "path": "a.txt", "offset": 1, "limit": 2 }),
        &ctx,
    );
    assert_eq!(result.content, "2\n3\n");
}

#[test]
fn bash_runs_and_captures_output() {
    let ctx = temp_ctx();
    let result = BashTool.run(&serde_json::json!({ "command": "echo hi" }), &ctx);
    assert!(!result.is_error);
    assert_eq!(result.content.trim(), "hi");
}

#[test]
fn bash_reports_failure_as_error() {
    let ctx = temp_ctx();
    let result = BashTool.run(&serde_json::json!({ "command": "exit 3" }), &ctx);
    assert!(result.is_error);
}

#[test]
fn ls_sorts_and_marks_directories() {
    let ctx = temp_ctx();
    std::fs::write(ctx.cwd.join("b.txt"), "x").expect("write");
    std::fs::create_dir_all(ctx.cwd.join("a_dir")).expect("mkdir");
    let result = LsTool.run(&serde_json::json!({}), &ctx);
    assert!(!result.is_error);
    assert_eq!(result.content, "a_dir/\nb.txt");
}

#[test]
fn tool_specs_serialize_for_the_provider() {
    let tools = default_tools();
    let specs = tool_specs(&tools);
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert_eq!(names, vec!["read", "bash", "ls"]);
    assert_eq!(
        specs[0].input_schema["required"],
        serde_json::json!(["path"])
    );
}
