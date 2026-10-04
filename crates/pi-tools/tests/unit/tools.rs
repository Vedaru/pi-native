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
    // pi's offset is 1-indexed: offset 2 starts at the second line.
    let result = ReadTool.run(
        &serde_json::json!({ "path": "a.txt", "offset": 2, "limit": 2 }),
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
        vec!["read", "bash", "edit", "write", "grep", "find", "ls"]
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

/// The default tools must declare exactly pi's name, description, and schema
/// (captured from `createAllToolDefinitions`, pi 1.0.2).
#[test]
fn default_tool_specs_match_pi() {
    let specs: Vec<serde_json::Value> = default_tools()
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name(),
                "description": tool.description(),
                "input_schema": tool.input_schema(),
            })
        })
        .collect();
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/default_tools.json")).expect("fixture");
    assert_eq!(serde_json::Value::Array(specs), expected);
}

#[test]
fn edit_accepts_pi_edits_array_and_rejects_overlap() {
    let ctx = temp_ctx();
    std::fs::write(ctx.cwd.join("a.txt"), "alpha beta gamma\n").expect("write");
    let result = EditTool.run(
        &serde_json::json!({
            "path": "a.txt",
            "edits": [
                { "oldText": "alpha", "newText": "ALPHA" },
                { "oldText": "gamma", "newText": "GAMMA" }
            ]
        }),
        &ctx,
    );
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        std::fs::read_to_string(ctx.cwd.join("a.txt")).unwrap(),
        "ALPHA beta GAMMA\n"
    );

    let overlap = EditTool.run(
        &serde_json::json!({
            "path": "a.txt",
            "edits": [
                { "oldText": "ALPHA beta", "newText": "x" },
                { "oldText": "beta GAMMA", "newText": "y" }
            ]
        }),
        &ctx,
    );
    assert!(overlap.is_error);
}

#[test]
fn grep_supports_literal_case_and_context() {
    let ctx = temp_ctx();
    std::fs::write(ctx.cwd.join("g.txt"), "a.b\naxb\nBETA\nbeta\ngamma").expect("write");

    let regex = GrepTool.run(&serde_json::json!({ "pattern": "a.b" }), &ctx);
    assert_eq!(regex.content.lines().count(), 2);
    let literal = GrepTool.run(
        &serde_json::json!({ "pattern": "a.b", "literal": true }),
        &ctx,
    );
    assert_eq!(literal.content, "g.txt:1: a.b");

    let insensitive = GrepTool.run(
        &serde_json::json!({ "pattern": "beta", "ignoreCase": true }),
        &ctx,
    );
    assert_eq!(insensitive.content, "g.txt:3: BETA\ng.txt:4: beta");

    // Context lines use `path-line- text`; the match uses `path:line: text`.
    let context = GrepTool.run(
        &serde_json::json!({ "pattern": "gamma", "context": 1 }),
        &ctx,
    );
    assert_eq!(context.content, "g.txt-4- beta\ng.txt:5: gamma");
}

#[test]
fn find_and_ls_honor_limit() {
    let ctx = temp_ctx();
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(ctx.cwd.join(name), "x").expect("write");
    }
    let found = FindTool.run(&serde_json::json!({ "pattern": "*.txt", "limit": 2 }), &ctx);
    assert_eq!(found.content.lines().count(), 2);
    let listed = LsTool.run(&serde_json::json!({ "limit": 2 }), &ctx);
    assert_eq!(listed.content.lines().count(), 2);
}
