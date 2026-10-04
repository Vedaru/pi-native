//! Native plugin host.
//!
//! This is the "wrapper" that connects the native Rust core to pi's plugin
//! ecosystem. It embeds QuickJS (`rquickjs`) and exposes a `pi` global whose
//! methods are hostcalls into Rust, mirroring pi's extension API.
//!
//! Following the reference port, plugins are JS/TS run in QuickJS; Node
//! built-ins are provided as Rust-backed virtual modules (added incrementally),
//! and npm bare specifiers resolve to stubs so a plugin can load and register
//! even when a library's behavior is absent.
//!
//! Stage 1: runtime + `pi` hostcall surface + capability check.
//! Stage 2: module resolver + virtual Node built-ins (`path`, `os`, `process`).

pub mod crypto;
pub mod globals;
pub mod modules;

use rquickjs::{Context, Ctx, Function, Module, Object, Runtime};
use std::sync::{Arc, Mutex};

/// Capabilities a plugin may request, mirroring pi's policy tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    Read,
    Write,
    Exec,
    Http,
    Session,
    Ui,
    Events,
    Log,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Read => "read",
            Capability::Write => "write",
            Capability::Exec => "exec",
            Capability::Http => "http",
            Capability::Session => "session",
            Capability::Ui => "ui",
            Capability::Events => "events",
            Capability::Log => "log",
        }
    }
}

/// A single hostcall recorded by a plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct HostCall {
    pub capability: Capability,
    pub method: String,
    /// JSON-encoded arguments (a single value or an array).
    pub args: serde_json::Value,
}

/// Policy for which capabilities a plugin may use. Deny-by-default.
#[derive(Debug, Clone, Default)]
pub struct PluginPolicy {
    allowed: std::collections::HashSet<Capability>,
}

impl PluginPolicy {
    /// Allow every capability (used by trusted, in-tree plugins and tests).
    pub fn permissive() -> Self {
        use Capability::*;
        Self {
            allowed: [Read, Write, Exec, Http, Session, Ui, Events, Log]
                .into_iter()
                .collect(),
        }
    }

    pub fn allow(mut self, capability: Capability) -> Self {
        self.allowed.insert(capability);
        self
    }

    pub fn is_allowed(&self, capability: Capability) -> bool {
        self.allowed.contains(&capability)
    }
}

#[derive(Debug)]
pub enum PluginError {
    Engine(String),
    Denied {
        capability: Capability,
        method: String,
    },
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::Engine(message) => write!(f, "plugin engine error: {message}"),
            PluginError::Denied { capability, method } => {
                write!(
                    f,
                    "plugin call denied: {method} requires '{}'",
                    capability.as_str()
                )
            }
        }
    }
}

impl std::error::Error for PluginError {}

impl From<rquickjs::Error> for PluginError {
    fn from(error: rquickjs::Error) -> Self {
        PluginError::Engine(error.to_string())
    }
}

/// Runs a plugin for the duration of one call and records its hostcalls.
pub struct PluginHost {
    policy: PluginPolicy,
    calls: Arc<Mutex<Vec<HostCall>>>,
    denials: Arc<Mutex<Vec<HostCall>>>,
}

impl PluginHost {
    pub fn new(policy: PluginPolicy) -> Self {
        Self {
            policy,
            calls: Arc::new(Mutex::new(Vec::new())),
            denials: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Execute plugin source that registers itself against the `pi` global.
    /// Returns the hostcalls the plugin made.
    pub fn run(&self, source: &str) -> Result<Vec<HostCall>, PluginError> {
        self.run_named("plugin://entry.js", source)
    }

    /// Execute plugin source under a filename so its extension selects the
    /// transpiler (`.ts`/`.tsx`/`.jsx` are transpiled with `swc`).
    pub fn run_named(&self, name: &str, source: &str) -> Result<Vec<HostCall>, PluginError> {
        let prepared = if pi_transpile::needs_transpile(name) {
            pi_transpile::transpile(name, source).map_err(PluginError::Engine)?
        } else {
            source.to_string()
        };

        let runtime = Runtime::new().map_err(PluginError::from)?;
        runtime.set_loader(modules::PiResolver, modules::PiLoader);
        let context = Context::full(&runtime).map_err(PluginError::from)?;

        context.with(|ctx| {
            self.install_pi_global(&ctx)?;
            self.install_host_fs(&ctx)?;
            self.install_host_crypto(&ctx)?;
            self.install_host_child_process(&ctx)?;
            install_env(&ctx)?;
            ctx.eval::<(), _>(globals::PRELUDE.as_bytes())?;
            let entry = Module::declare(ctx.clone(), name, prepared.as_bytes())?;
            // Module bodies run synchronously; top-level await is not supported yet.
            let _ = entry.eval()?;
            Ok::<(), PluginError>(())
        })?;

        if let Some(denied) = self.denials.lock().expect("denials lock").first().cloned() {
            return Err(PluginError::Denied {
                capability: denied.capability,
                method: denied.method,
            });
        }

        let calls = self.calls.lock().expect("calls lock").clone();
        Ok(calls)
    }

    fn install_pi_global(&self, ctx: &Ctx<'_>) -> Result<(), PluginError> {
        let pi = Object::new(ctx.clone())?;

        // pi.log(entry)
        let calls = self.calls.clone();
        let denials = self.denials.clone();
        let policy = self.policy.clone();
        let log_fn = Function::new(ctx.clone(), move |entry: rquickjs::Value<'_>| {
            record(
                &policy,
                &calls,
                &denials,
                Capability::Log,
                "log",
                json_from_js(&entry),
            );
        })?;
        pi.set("log", log_fn)?;

        // pi.exec(command, args?) -> subprocess hostcall
        let calls = self.calls.clone();
        let denials = self.denials.clone();
        let policy = self.policy.clone();
        let exec_fn = Function::new(
            ctx.clone(),
            move |command: String, args: Option<Vec<String>>| {
                let args = args.unwrap_or_default();
                record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Exec,
                    "exec",
                    serde_json::json!({ "command": command, "args": args }),
                );
            },
        )?;
        pi.set("exec", exec_fn)?;

        // pi.tool(name, input)
        let calls = self.calls.clone();
        let denials = self.denials.clone();
        let policy = self.policy.clone();
        let tool_fn = Function::new(
            ctx.clone(),
            move |name: String, input: rquickjs::Value<'_>| {
                record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Read,
                    "tool",
                    serde_json::json!({ "name": name, "input": json_from_js(&input) }),
                );
            },
        )?;
        pi.set("tool", tool_fn)?;

        // pi.registerTool(spec) / registerCommand / registerProvider
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        pi.set(
            "registerTool",
            Function::new(ctx.clone(), move |spec: rquickjs::Value<'_>| {
                record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Events,
                    "registerTool",
                    json_from_js(&spec),
                );
            }),
        )?;

        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        pi.set(
            "registerCommand",
            Function::new(
                ctx.clone(),
                move |name: String, spec: rquickjs::Value<'_>| {
                    record(
                        &policy,
                        &calls,
                        &denials,
                        Capability::Events,
                        "registerCommand",
                        serde_json::json!({ "name": name, "spec": json_from_js(&spec) }),
                    );
                },
            ),
        )?;

        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        pi.set(
            "registerProvider",
            Function::new(ctx.clone(), move |spec: rquickjs::Value<'_>| {
                record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Events,
                    "registerProvider",
                    json_from_js(&spec),
                );
            }),
        )?;

        // pi.session(op, args) / pi.ui(op, args) / pi.events(op, args)
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        pi.set(
            "session",
            Function::new(ctx.clone(), move |op: String, args: rquickjs::Value<'_>| {
                record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Session,
                    "session",
                    serde_json::json!({ "op": op, "args": json_from_js(&args) }),
                );
            }),
        )?;

        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        pi.set(
            "ui",
            Function::new(ctx.clone(), move |op: String, args: rquickjs::Value<'_>| {
                record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Ui,
                    "ui",
                    serde_json::json!({ "op": op, "args": json_from_js(&args) }),
                );
            }),
        )?;

        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        pi.set(
            "events",
            Function::new(ctx.clone(), move |op: String, args: rquickjs::Value<'_>| {
                record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Events,
                    "events",
                    serde_json::json!({ "op": op, "args": json_from_js(&args) }),
                );
            }),
        )?;

        ctx.globals().set("pi", pi)?;
        Ok(())
    }

    /// Install `__pi_host.fs`, the capability-gated filesystem the virtual
    /// `node:fs` module delegates to.
    fn install_host_fs(&self, ctx: &Ctx<'_>) -> Result<(), PluginError> {
        let host = Object::new(ctx.clone())?;
        let fs = Object::new(ctx.clone())?;

        // readFileSync(path, encoding?) -> string
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        fs.set(
            "readFileSync",
            Function::new(
                ctx.clone(),
                move |path: String, _encoding: Option<String>| -> String {
                    let allowed = record(
                        &policy,
                        &calls,
                        &denials,
                        Capability::Read,
                        "fs.readFileSync",
                        serde_json::json!({ "path": path }),
                    );
                    if allowed {
                        std::fs::read_to_string(&path).unwrap_or_default()
                    } else {
                        String::new()
                    }
                },
            ),
        )?;

        // writeFileSync(path, data)
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        fs.set(
            "writeFileSync",
            Function::new(ctx.clone(), move |path: String, data: String| {
                if record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Write,
                    "fs.writeFileSync",
                    serde_json::json!({ "path": path }),
                ) {
                    let _ = std::fs::write(&path, data);
                }
            }),
        )?;

        // existsSync(path) -> bool
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        fs.set(
            "existsSync",
            Function::new(ctx.clone(), move |path: String| -> bool {
                let allowed = record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Read,
                    "fs.existsSync",
                    serde_json::json!({ "path": path }),
                );
                allowed && std::path::Path::new(&path).exists()
            }),
        )?;

        // readdirSync(path) -> string[]
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        fs.set(
            "readdirSync",
            Function::new(ctx.clone(), move |path: String| -> Vec<String> {
                let allowed = record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Read,
                    "fs.readdirSync",
                    serde_json::json!({ "path": path }),
                );
                if !allowed {
                    return Vec::new();
                }
                std::fs::read_dir(&path)
                    .map(|entries| {
                        entries
                            .flatten()
                            .map(|e| e.file_name().to_string_lossy().to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            }),
        )?;

        // mkdirSync(path, { recursive }?)
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        fs.set(
            "mkdirSync",
            Function::new(ctx.clone(), move |path: String, recursive: Option<bool>| {
                if record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Write,
                    "fs.mkdirSync",
                    serde_json::json!({ "path": path }),
                ) {
                    let _ = if recursive.unwrap_or(false) {
                        std::fs::create_dir_all(&path)
                    } else {
                        std::fs::create_dir(&path)
                    };
                }
            }),
        )?;

        // unlinkSync(path)
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        fs.set(
            "unlinkSync",
            Function::new(ctx.clone(), move |path: String| {
                if record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Write,
                    "fs.unlinkSync",
                    serde_json::json!({ "path": path }),
                ) {
                    let _ = std::fs::remove_file(&path);
                }
            }),
        )?;

        host.set("fs", fs)?;
        ctx.globals().set("__pi_host", host)?;
        Ok(())
    }

    /// Install `__pi_host.crypto`, pure local primitives (no policy gate: these
    /// are computation, not ambient side effects).
    fn install_host_crypto(&self, ctx: &Ctx<'_>) -> Result<(), PluginError> {
        let globals = ctx.globals();
        let host: Object = globals.get("__pi_host")?;
        let crypto = Object::new(ctx.clone())?;

        crypto.set(
            "hashBytes",
            Function::new(ctx.clone(), |algo: String, data: String| -> Vec<u8> {
                crate::crypto::hash_bytes(&algo, data.as_bytes()).unwrap_or_default()
            }),
        )?;
        crypto.set(
            "hmacBytes",
            Function::new(
                ctx.clone(),
                |algo: String, key: String, data: String| -> Vec<u8> {
                    crate::crypto::hmac_bytes(&algo, key.as_bytes(), data.as_bytes())
                        .unwrap_or_default()
                },
            ),
        )?;
        crypto.set(
            "randomBytes",
            Function::new(ctx.clone(), |len: u32| -> Vec<u8> {
                crate::crypto::random_bytes(len as usize)
            }),
        )?;
        crypto.set(
            "randomUUID",
            Function::new(ctx.clone(), || -> String { crate::crypto::random_uuid() }),
        )?;

        host.set("crypto", crypto)?;
        Ok(())
    }

    /// Install `__pi_host.child_process`, gated by the `exec` capability.
    fn install_host_child_process(&self, ctx: &Ctx<'_>) -> Result<(), PluginError> {
        let globals = ctx.globals();
        let host: Object = globals.get("__pi_host")?;
        let cp = Object::new(ctx.clone())?;

        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        cp.set(
            "execShell",
            Function::new(ctx.clone(), move |command: String| -> String {
                let allowed = record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Exec,
                    "child_process.exec",
                    serde_json::json!({ "command": command }),
                );
                if allowed {
                    run_shell(&command)
                } else {
                    String::new()
                }
            }),
        )?;

        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        cp.set(
            "spawnSync",
            Function::new(
                ctx.clone(),
                move |command: String, args: Vec<String>| -> String {
                    let allowed = record(
                        &policy,
                        &calls,
                        &denials,
                        Capability::Exec,
                        "child_process.spawnSync",
                        serde_json::json!({ "command": command, "args": args }),
                    );
                    if allowed {
                        run_command(&command, &args)
                    } else {
                        serde_json::json!({ "status": null, "stdout": "", "stderr": "denied" })
                            .to_string()
                    }
                },
            ),
        )?;

        host.set("child_process", cp)?;
        Ok(())
    }
}

/// Run a shell command, returning stdout (lossy).
fn run_shell(command: &str) -> String {
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
        Err(_) => String::new(),
    }
}

/// Run a command directly, returning JSON `{status, stdout, stderr}`.
fn run_command(command: &str, args: &[String]) -> String {
    match std::process::Command::new(command).args(args).output() {
        Ok(output) => serde_json::json!({
            "status": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        })
        .to_string(),
        Err(error) => serde_json::json!({
            "status": serde_json::Value::Null,
            "stdout": "",
            "stderr": error.to_string(),
        })
        .to_string(),
    }
}

fn platform_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Inject the small host environment the virtual `os`/`process` modules read.
fn install_env(ctx: &Ctx<'_>) -> Result<(), PluginError> {
    let env = Object::new(ctx.clone())?;
    env.set("platform", platform_name())?;
    env.set("arch", std::env::consts::ARCH)?;
    env.set("homedir", std::env::var("HOME").unwrap_or_default())?;
    env.set("tmpdir", std::env::temp_dir().to_string_lossy().to_string())?;
    env.set("eol", "\n")?;
    env.set(
        "cwd",
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
    )?;
    ctx.globals().set("__pi_env", env)?;
    Ok(())
}

fn json_from_js(value: &rquickjs::Value<'_>) -> serde_json::Value {
    js_to_json(value, 0)
}

/// Convert a JS value to JSON. Functions are skipped at object keys and become
/// `null` elsewhere, so `registerTool({ execute })` keeps its metadata without
/// trying to serialize the handler.
fn js_to_json(value: &rquickjs::Value<'_>, depth: usize) -> serde_json::Value {
    if depth > 8 || value.is_undefined() || value.is_null() || value.is_function() {
        return serde_json::Value::Null;
    }
    if let Some(b) = value.as_bool() {
        return serde_json::Value::Bool(b);
    }
    if let Some(i) = value.as_int() {
        return serde_json::json!(i);
    }
    if let Some(f) = value.as_float() {
        return serde_json::json!(f);
    }
    if let Some(s) = value.as_string() {
        return serde_json::Value::String(s.to_string().unwrap_or_default());
    }
    if let Some(array) = value.as_array() {
        let mut out = Vec::new();
        for item in array.iter::<rquickjs::Value<'_>>().flatten() {
            out.push(js_to_json(&item, depth + 1));
        }
        return serde_json::Value::Array(out);
    }
    if let Some(object) = value.as_object() {
        let mut map = serde_json::Map::new();
        for key in object.keys::<String>().flatten() {
            let Ok(item) = object.get::<_, rquickjs::Value<'_>>(&key) else {
                continue;
            };
            if item.is_function() || item.is_undefined() {
                continue;
            }
            map.insert(key, js_to_json(&item, depth + 1));
        }
        return serde_json::Value::Object(map);
    }
    serde_json::Value::Null
}

fn record(
    policy: &PluginPolicy,
    calls: &Arc<Mutex<Vec<HostCall>>>,
    denials: &Arc<Mutex<Vec<HostCall>>>,
    capability: Capability,
    method: &str,
    args: serde_json::Value,
) -> bool {
    let call = HostCall {
        capability,
        method: method.to_string(),
        args,
    };
    if policy.is_allowed(capability) {
        calls.lock().expect("calls lock").push(call);
        true
    } else {
        denials.lock().expect("denials lock").push(call);
        false
    }
}

#[cfg(test)]
mod tests {
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
            .run(r#"import { execSync } from "node:child_process"; pi.log(execSync("echo hi").trim());"#)
            .expect("runs");
        assert_eq!(calls.last().unwrap().args, serde_json::json!("hi"));
    }

    #[test]
    fn child_process_is_denied_by_default_policy() {
        let host = PluginHost::new(PluginPolicy::default());
        let result =
            host.run(r#"import { execSync } from "node:child_process"; execSync("echo hi");"#);
        match result {
            Err(PluginError::Denied { capability, .. }) => assert_eq!(capability, Capability::Exec),
            other => panic!("expected exec denial, got {other:?}"),
        }
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
}
