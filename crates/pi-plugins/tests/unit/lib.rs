use super::*;

#[test]
fn records_a_log_hostcall() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host.run(r#"pi.log("hello from plugin");"#).expect("runs");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].capability, Capability::Log);
    assert_eq!(calls[0].method, "log");
    assert_eq!(calls[0].args, serde_json::json!("hello from plugin"));
}

#[test]
fn records_exec_with_arguments() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host.run(r#"pi.exec("ls", ["-la"]);"#).expect("runs");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].capability, Capability::Exec);
    assert_eq!(
        calls[0].args,
        serde_json::json!({"command": "ls", "args": ["-la"]})
    );
}

#[test]
fn denied_capability_is_reported() {
    // Default policy allows nothing.
    let host = PluginHost::new(PluginPolicy::default());
    let result = host.run(r#"pi.log("denied");"#);
    assert!(result.is_err());
    match result.unwrap_err() {
        PluginError::Denied { capability, .. } => assert_eq!(capability, Capability::Log),
        other => panic!("expected denial, got {other:?}"),
    }
}

#[test]
fn plugin_can_compute_before_calling_out() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(r#"const n = 2 + 3; pi.log("n=" + n);"#)
        .expect("runs");
    assert_eq!(calls[0].args, serde_json::json!("n=5"));
}

#[test]
fn plugin_can_import_node_path() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(r#"import path from "node:path"; pi.log(path.join("a", "b"));"#)
        .expect("runs");
    assert_eq!(calls[0].args, serde_json::json!("a/b"));
}

#[test]
fn plugin_can_import_os_without_prefix() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(r#"import os from "os"; pi.log(os.platform());"#)
        .expect("runs");
    assert_eq!(calls[0].args, serde_json::json!(platform_name()));
}

#[test]
fn bare_npm_import_loads_as_a_stub() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(r#"import pkg from "some-npm-pkg"; pkg.anything(); pi.log("loaded");"#)
        .expect("runs");
    assert_eq!(calls[0].args, serde_json::json!("loaded"));
}

#[test]
fn runs_a_typescript_entrypoint() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run_named(
            "plugin.ts",
            r#"const n: number = 41; pi.log("n=" + (n + 1));"#,
        )
        .expect("runs");
    assert_eq!(calls[0].args, serde_json::json!("n=42"));
}

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("pi-native-test-{}-{name}", std::process::id()));
    path
}

#[test]
fn plugin_can_read_a_file_through_the_host() {
    let path = temp_path("input.txt");
    std::fs::write(&path, "file-contents").expect("write fixture");
    let host = PluginHost::new(PluginPolicy::permissive());
    let source = format!(
        r#"import fs from "node:fs"; pi.log(fs.readFileSync({:?}, "utf8"));"#,
        path.to_string_lossy()
    );
    let calls = host.run(&source).expect("runs");
    let last = calls.last().expect("a call");
    assert_eq!(last.args, serde_json::json!("file-contents"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn fs_read_is_denied_by_default_policy() {
    let host = PluginHost::new(PluginPolicy::default());
    let result = host.run(r#"import fs from "node:fs"; fs.readFileSync("/etc/hostname");"#);
    match result {
        Err(PluginError::Denied { capability, .. }) => assert_eq!(capability, Capability::Read),
        other => panic!("expected read denial, got {other:?}"),
    }
}

#[test]
fn records_a_tool_registration_without_the_handler() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(
            r#"pi.registerTool({ name: "greet", description: "Say hi", parameters: { type: "object" }, execute: () => 1 });"#,
        )
        .expect("runs");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method, "registerTool");
    assert_eq!(calls[0].args["name"], serde_json::json!("greet"));
    assert_eq!(calls[0].args["description"], serde_json::json!("Say hi"));
    assert_eq!(
        calls[0].args["parameters"]["type"],
        serde_json::json!("object")
    );
    assert!(
        calls[0].args.get("execute").is_none(),
        "handler should be omitted"
    );
}

#[test]
fn records_command_registration() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(r#"pi.registerCommand("hello", { description: "Greet" });"#)
        .expect("runs");
    assert_eq!(calls[0].method, "registerCommand");
    assert_eq!(calls[0].args["name"], serde_json::json!("hello"));
    assert_eq!(
        calls[0].args["spec"]["description"],
        serde_json::json!("Greet")
    );
}

#[test]
fn records_session_and_ui_hostcalls() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(r#"pi.session("getState", {}); pi.ui("notify", "hello");"#)
        .expect("runs");
    assert_eq!(calls[0].capability, Capability::Session);
    assert_eq!(calls[0].args["op"], serde_json::json!("getState"));
    assert_eq!(calls[1].capability, Capability::Ui);
    assert_eq!(calls[1].args["args"], serde_json::json!("hello"));
}

#[test]
fn plugin_can_hash_with_node_crypto() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(
            r#"import { createHash } from "node:crypto"; pi.log(createHash("sha256").update("abc").digest("hex"));"#,
        )
        .expect("runs");
    assert_eq!(
        calls[0].args,
        serde_json::json!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
}

#[test]
fn plugin_can_generate_a_uuid() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(r#"import { randomUUID } from "node:crypto"; pi.log(randomUUID());"#)
        .expect("runs");
    assert_eq!(calls[0].args.as_str().map(str::len), Some(36));
}

#[test]
fn plugin_can_use_event_emitter() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(
            r#"import { EventEmitter } from "node:events"; const e = new EventEmitter(); e.on("x", (v) => pi.log("got " + v)); e.emit("x", 5);"#,
        )
        .expect("runs");
    assert_eq!(calls[0].args, serde_json::json!("got 5"));
}

#[test]
fn plugin_can_run_exec_sync() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(
            r#"import { execSync } from "node:child_process"; pi.log(execSync("echo hi").trim());"#,
        )
        .expect("runs");
    assert_eq!(calls.last().unwrap().args, serde_json::json!("hi"));
}

#[test]
fn child_process_is_denied_by_default_policy() {
    let host = PluginHost::new(PluginPolicy::default());
    let result = host.run(r#"import { execSync } from "node:child_process"; execSync("echo hi");"#);
    match result {
        Err(PluginError::Denied { capability, .. }) => assert_eq!(capability, Capability::Exec),
        other => panic!("expected exec denial, got {other:?}"),
    }
}

#[test]
fn extension_factory_receives_the_api() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(
            r#"export default function (pi) { pi.registerTool({ name: "x", description: "d" }); }"#,
        )
        .expect("runs");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method, "registerTool");
    assert_eq!(calls[0].args["name"], serde_json::json!("x"));
}

#[test]
fn plugin_can_use_buffer_hex_and_base64() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(
            r#"pi.log(Buffer.from("hello").toString("hex")); pi.log(Buffer.from("hi").toString("base64"));"#,
        )
        .expect("runs");
    assert_eq!(calls[0].args, serde_json::json!("68656c6c6f"));
    assert_eq!(calls[1].args, serde_json::json!("aGk="));
}
