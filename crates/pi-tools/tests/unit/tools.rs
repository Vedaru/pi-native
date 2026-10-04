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
    assert_eq!(
        names,
        vec!["read", "bash", "ls", "write", "edit", "grep", "find"]
    );
    assert_eq!(
        specs[0].input_schema["required"],
        serde_json::json!(["path"])
    );
}

#[test]
fn write_creates_a_file_and_directories() {
    let ctx = temp_ctx();
    let result = WriteTool.run(
        &serde_json::json!({ "path": "nested/dir/a.txt", "content": "hello" }),
        &ctx,
    );
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        std::fs::read_to_string(ctx.cwd.join("nested/dir/a.txt")).unwrap(),
        "hello"
    );
}

#[test]
fn edit_replaces_a_unique_string() {
    let ctx = temp_ctx();
    std::fs::write(ctx.cwd.join("a.txt"), "one\ntwo\nthree\n").expect("write");
    let result = EditTool.run(
        &serde_json::json!({ "path": "a.txt", "oldText": "two", "newText": "TWO" }),
        &ctx,
    );
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        std::fs::read_to_string(ctx.cwd.join("a.txt")).unwrap(),
        "one\nTWO\nthree\n"
    );
}

#[test]
fn edit_rejects_a_non_unique_string() {
    let ctx = temp_ctx();
    std::fs::write(ctx.cwd.join("a.txt"), "x x x").expect("write");
    let result = EditTool.run(
        &serde_json::json!({ "path": "a.txt", "oldText": "x", "newText": "y" }),
        &ctx,
    );
    assert!(result.is_error);
    assert!(result.content.contains("not unique"));
}

#[test]
fn grep_finds_matching_lines() {
    let ctx = temp_ctx();
    std::fs::write(ctx.cwd.join("a.txt"), "alpha\nbeta\n").expect("write");
    std::fs::write(ctx.cwd.join("b.txt"), "gamma\n").expect("write");
    let result = GrepTool.run(&serde_json::json!({ "pattern": "a$" }), &ctx);
    assert!(!result.is_error, "{result:?}");
    assert!(result.content.contains("a.txt:1: alpha"), "{result:?}");
    assert!(result.content.contains("b.txt:1: gamma"), "{result:?}");
}

#[test]
fn find_matches_a_glob() {
    let ctx = temp_ctx();
    std::fs::write(ctx.cwd.join("a.rs"), "").expect("write");
    std::fs::write(ctx.cwd.join("b.txt"), "").expect("write");
    let result = FindTool.run(&serde_json::json!({ "pattern": "*.rs" }), &ctx);
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.content, "a.rs");
}

#[test]
fn read_bounds_a_large_file() {
    let ctx = temp_ctx();
    std::fs::write(ctx.cwd.join("big.txt"), "x".repeat(2 * 1024 * 1024)).expect("write");
    let result = ReadTool.run(&serde_json::json!({ "path": "big.txt" }), &ctx);
    assert!(!result.is_error, "{result:?}");
    assert!(
        result.content.len() < DEFAULT_MAX_BYTES + 256,
        "read did not bound output: {}",
        result.content.len()
    );
}

#[test]
fn grep_streams_a_large_file() {
    let ctx = temp_ctx();
    let mut content = "x".repeat(1024 * 1024);
    content.push_str("\nneedle\n");
    std::fs::write(ctx.cwd.join("big.txt"), content).expect("write");
    let result = GrepTool.run(&serde_json::json!({ "pattern": "needle" }), &ctx);
    assert!(!result.is_error, "{result:?}");
    assert!(result.content.contains("big.txt:2: needle"), "{result:?}");
}
