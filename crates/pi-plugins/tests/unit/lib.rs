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

#[test]
fn instance_registers_and_calls_a_tool() {
    let source = r#"
        export default function (pi) {
            pi.registerTool({
                name: "greet",
                description: "Greet someone",
                parameters: { type: "object", properties: { who: { type: "string" } } },
                execute: (input) => ({ content: [{ type: "text", text: "hello " + input.who }] }),
            });
            pi.registerCommand("shout", { description: "Shout" });
        }
    "#;
    let instance =
        PluginInstance::load(PluginPolicy::permissive(), "plugin://test.ts", source).expect("load");
    assert_eq!(instance.tools().len(), 1);
    assert_eq!(instance.tools()[0].name, "greet");
    assert_eq!(instance.commands().len(), 1);

    let result = instance
        .call_tool("greet", &serde_json::json!({ "who": "world" }))
        .expect("call");
    assert_eq!(
        result["content"][0]["text"],
        serde_json::json!("hello world")
    );
}

#[test]
fn large_integers_reach_js_without_truncation() {
    // 2^53 + 1 would round through an f64 and overflow an i32; the host must
    // hand QuickJS an exact value.
    let source = r#"
        export default function (pi) {
            pi.registerTool({
                name: "echo",
                description: "Echo",
                parameters: { type: "object" },
                execute: (input) => ({ content: [{ type: "text", text: String(input.big) }] }),
            });
        }
    "#;
    let instance =
        PluginInstance::load(PluginPolicy::permissive(), "plugin://big.ts", source).expect("load");
    let result = instance
        .call_tool("echo", &serde_json::json!({ "big": 9007199254740993i64 }))
        .expect("call");
    assert_eq!(
        result["content"][0]["text"],
        serde_json::json!("9007199254740993")
    );
}

#[test]
fn path_resolve_uses_the_injected_cwd() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(r#"import path from "node:path"; pi.log(path.resolve("a", "b"));"#)
        .expect("runs");
    let cwd = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert_eq!(calls[0].args, serde_json::json!(format!("{cwd}/a/b")));
}

#[test]
fn process_env_is_populated() {
    std::env::set_var("PI_NATIVE_TEST_ENV", "hello");
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(r#"import process from "node:process"; pi.log(process.env.PI_NATIVE_TEST_ENV);"#)
        .expect("runs");
    assert_eq!(calls[0].args, serde_json::json!("hello"));
}

#[test]
fn node_http_and_https_modules_load() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(
            r#"import http from "node:http"; import https from "node:https"; pi.log(typeof http.request + "/" + typeof https.get);"#,
        )
        .expect("runs");
    assert_eq!(calls[0].args, serde_json::json!("function/function"));
}

#[test]
fn extension_policy_denies_ambient_capabilities_by_default() {
    let root = std::env::temp_dir();
    let host = PluginHost::new(PluginPolicy::for_extension(&root));
    let result = host.run(r#"import { execSync } from "node:child_process"; execSync("echo hi");"#);
    match result {
        Err(PluginError::Denied { capability, .. }) => assert_eq!(capability, Capability::Exec),
        other => panic!("expected exec denial, got {other:?}"),
    }
}

#[test]
fn extension_policy_jails_granted_reads_to_the_workspace() {
    let root = std::env::temp_dir().join(format!("pi-plugin-root-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let inside = root.join("in.txt");
    std::fs::write(&inside, "inside").expect("write inside");
    let outside =
        std::env::temp_dir().join(format!("pi-plugin-outside-{}.txt", std::process::id()));
    std::fs::write(&outside, "outside").expect("write outside");

    let host = PluginHost::new(PluginPolicy::for_extension(&root).allow(Capability::Read));
    let allowed = host
        .run(&format!(
            r#"import fs from "node:fs"; pi.log(fs.readFileSync({:?}, "utf8"));"#,
            inside.to_string_lossy()
        ))
        .expect("inside reads");
    assert_eq!(allowed.last().unwrap().args, serde_json::json!("inside"));

    let escaped = host.run(&format!(
        r#"import fs from "node:fs"; fs.readFileSync({:?}, "utf8");"#,
        outside.to_string_lossy()
    ));
    match escaped {
        Err(PluginError::Denied { capability, .. }) => assert_eq!(capability, Capability::Read),
        other => panic!("expected jail denial, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&outside);
}

#[test]
fn escaping_write_is_a_hard_error_and_records_a_denial() {
    let root = std::env::temp_dir().join(format!("pi-plugin-write-root-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let outside =
        std::env::temp_dir().join(format!("pi-plugin-write-outside-{}.", std::process::id()));

    let host = PluginHost::new(PluginPolicy::for_extension(&root).allow(Capability::Write));
    let escaped = host.run(&format!(
        r#"import fs from "node:fs"; fs.writeFileSync({:?}, "pwned");"#,
        outside.to_string_lossy()
    ));
    // A silent no-op is not acceptable for a mutating call: the escape must
    // fail the run rather than return as if the write succeeded. At module
    // evaluation the recorded denial surfaces as `Denied`; in synchronous
    // tool execution it surfaces as `Engine`.
    match escaped {
        Err(PluginError::Denied { capability, .. }) => {
            assert_eq!(capability, Capability::Write);
        }
        Err(PluginError::Engine(message)) => {
            assert!(message.contains("escapes"), "unexpected message: {message}");
        }
        other => panic!("expected an escaping-write failure, got {other:?}"),
    }
    assert!(!outside.exists(), "the escaping write must not happen");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn escaping_write_in_tool_execution_is_a_hard_error() {
    let root = std::env::temp_dir().join(format!("pi-plugin-tool-root-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let outside =
        std::env::temp_dir().join(format!("pi-plugin-tool-outside-{}.", std::process::id()));

    let source = format!(
        r#"
        export default function (pi) {{
            pi.registerTool({{
                name: "leak",
                description: "Write outside the workspace",
                parameters: {{ type: "object" }},
                execute: () => {{
                    const fs = require("node:fs");
                    fs.writeFileSync({:?}, "pwned");
                    return {{ content: [] }};
                }},
            }});
        }}
    "#,
        outside.to_string_lossy()
    );
    let instance = PluginInstance::load(
        PluginPolicy::for_extension(&root).allow(Capability::Write),
        "plugin://leak.js",
        &source,
    )
    .expect("load");

    let result = instance.call_tool("leak", &serde_json::json!({}));
    assert!(result.is_err(), "tool must fail on an escaping write");
    assert!(!outside.exists(), "the escaping write must not happen");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn random_bytes_is_capped() {
    // A plugin must not be able to request a multi-GB allocation.
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(r#"import { randomBytes } from "node:crypto"; pi.log(randomBytes(2 ** 31).length);"#)
        .expect("runs");
    assert_eq!(calls[0].args, serde_json::json!(16 * 1024 * 1024));
}

#[test]
fn buffer_alloc_with_empty_fill_does_not_throw() {
    let host = PluginHost::new(PluginPolicy::permissive());
    let calls = host
        .run(r#"pi.log(Buffer.alloc(4, "").toString("hex"));"#)
        .expect("runs");
    assert_eq!(calls[0].args, serde_json::json!("00000000"));
}

#[test]
fn read_file_sync_throws_on_a_failed_read() {
    // An empty file and a failed read must be distinguishable: a missing file
    // throws instead of returning an empty string. Exercise it through a tool
    // call (synchronous), where a thrown hostcall error propagates to the
    // caller.
    let root = std::env::temp_dir().join(format!("pi-plugin-read-root-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let missing = root.join("does-not-exist.txt");
    let source = format!(
        r#"
        export default function (pi) {{
            pi.registerTool({{
                name: "read",
                description: "Read a missing file",
                parameters: {{ type: "object" }},
                execute: () => {{
                    const fs = require("node:fs");
                    return fs.readFileSync({:?}, "utf8");
                }},
            }});
        }}
    "#,
        missing.to_string_lossy()
    );
    let instance = PluginInstance::load(
        PluginPolicy::for_extension(&root).allow(Capability::Read),
        "plugin://read.js",
        &source,
    )
    .expect("load");
    let result = instance.call_tool("read", &serde_json::json!({}));
    assert!(
        matches!(result, Err(PluginError::Engine(_))),
        "a failed read must throw, got {result:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn top_level_hostcall_error_makes_the_module_load_fail() {
    // A hostcall that throws at module scope must fail the load with the real
    // error, not be swallowed into the ignored module promise (VED-358).
    let root = std::env::temp_dir().join(format!("pi-plugin-top-root-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let missing = root.join("missing.txt");

    let host = PluginHost::new(PluginPolicy::for_extension(&root).allow(Capability::Read));
    let result = host.run(&format!(
        r#"import fs from "node:fs"; fs.readFileSync({:?}, "utf8");"#,
        missing.to_string_lossy()
    ));
    match result {
        Err(PluginError::Engine(message)) => {
            assert!(
                message.contains("readFileSync"),
                "the real error must surface: {message}"
            );
        }
        other => panic!("expected a top-level hostcall failure, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn register_message_renderer_is_recorded_and_callable() {
    let source = r#"
        export default function (pi) {
            pi.registerMessageRenderer("boxed", (entry) => [
                "+" + "-".repeat(entry.width) + "+",
                "| " + entry.text + " |",
                "+" + "-".repeat(entry.width) + "+",
            ]);
        }
    "#;
    let instance = PluginInstance::load(PluginPolicy::permissive(), "plugin://render.ts", source)
        .expect("load");
    assert_eq!(instance.renderers().len(), 1);
    assert_eq!(instance.renderers()[0].kind, "message");
    assert_eq!(instance.renderers()[0].name, "boxed");

    let lines = instance
        .render_lines("message", "boxed", 3, &serde_json::json!({ "text": "hi" }))
        .expect("render");
    assert_eq!(lines, vec!["+---+", "| hi |", "+---+"]);

    let missing = instance.render_lines("message", "nope", 3, &serde_json::json!({}));
    assert!(missing.is_err());
}

#[test]
fn register_markdown_transformer_receives_width_and_returns_lines() {
    let source = r#"
        export default function (pi) {
            pi.registerMarkdownTransformer("upper", (input) => {
                const text = typeof input.value === "string" ? input.value : "";
                return text.toUpperCase().split("\n");
            });
        }
    "#;
    let instance =
        PluginInstance::load(PluginPolicy::permissive(), "plugin://md.ts", source).expect("load");
    assert_eq!(instance.renderers()[0].kind, "markdown");

    let lines = instance
        .render_lines("markdown", "upper", 40, &serde_json::json!("hello\nworld"))
        .expect("render");
    assert_eq!(lines, vec!["HELLO", "WORLD"]);
}

#[test]
fn render_lines_accepts_a_single_string() {
    let source = r#"
        export default function (pi) {
            pi.registerMarkdownTransformer("plain", () => "one\ntwo");
        }
    "#;
    let instance = PluginInstance::load(PluginPolicy::permissive(), "plugin://plain.ts", source)
        .expect("load");
    let lines = instance
        .render_lines("markdown", "plain", 10, &serde_json::json!(null))
        .expect("render");
    assert_eq!(lines, vec!["one", "two"]);
}

#[test]
fn message_renderer_may_return_a_component_object() {
    // pi's `registerMessageRenderer(name, fn)` returns a Component with a
    // `render(width)` method; the bridge must call that method, not stringify
    // the object.
    let source = r#"
        export default function (pi) {
            pi.registerMessageRenderer("component", (message) => ({
                render: (width) => [
                    "[" + width + "]",
                    message.content,
                ],
                invalidate() {},
            }));
        }
    "#;
    let instance =
        PluginInstance::load(PluginPolicy::permissive(), "plugin://comp.ts", source).expect("load");
    let lines = instance
        .render_lines(
            "message",
            "component",
            24,
            &serde_json::json!({ "content": "hi" }),
        )
        .expect("render");
    assert_eq!(lines, vec!["[24]", "hi"]);
}

#[test]
fn markdown_transformer_component_receives_width() {
    let source = r#"
        export default function (pi) {
            pi.registerMarkdownTransformer("box", () => ({
                render: (width) => ["+" + "-".repeat(width) + "+"],
            }));
        }
    "#;
    let instance = PluginInstance::load(PluginPolicy::permissive(), "plugin://mdbox.ts", source)
        .expect("load");
    let lines = instance
        .render_lines("markdown", "box", 5, &serde_json::json!("x"))
        .expect("render");
    assert_eq!(lines, vec!["+-----+"]);
}
