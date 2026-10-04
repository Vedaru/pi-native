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

use rquickjs::{CatchResultExt, Context, Ctx, Function, Module, Object, Runtime};
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

/// A tool an extension registered via `pi.registerTool`.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Runs a plugin for the duration of one call and records its hostcalls.
pub struct PluginHost {
    policy: PluginPolicy,
    calls: Arc<Mutex<Vec<HostCall>>>,
    denials: Arc<Mutex<Vec<HostCall>>>,
    tools: Arc<Mutex<Vec<PluginToolSpec>>>,
    commands: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl PluginHost {
    pub fn new(policy: PluginPolicy) -> Self {
        Self {
            policy,
            calls: Arc::new(Mutex::new(Vec::new())),
            denials: Arc::new(Mutex::new(Vec::new())),
            tools: Arc::new(Mutex::new(Vec::new())),
            commands: Arc::new(Mutex::new(Vec::new())),
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
        self.evaluate(&context, name, &prepared)?;

        if let Some(denied) = self.denials.lock().expect("denials lock").first().cloned() {
            return Err(PluginError::Denied {
                capability: denied.capability,
                method: denied.method,
            });
        }

        let calls = self.calls.lock().expect("calls lock").clone();
        Ok(calls)
    }

    /// Install globals, evaluate the module, and invoke its default factory.
    fn evaluate(&self, context: &Context, name: &str, prepared: &str) -> Result<(), PluginError> {
        context.with(|ctx| {
            self.install_pi_global(&ctx)?;
            self.install_host_fs(&ctx)?;
            self.install_host_crypto(&ctx)?;
            self.install_host_child_process(&ctx)?;
            self.install_host_zlib(&ctx)?;
            self.install_host_api_call(&ctx)?;
            install_env(&ctx)?;
            ctx.eval::<(), _>(globals::PRELUDE.as_bytes())
                .catch(&ctx)
                .map_err(|error| caught_error("prelude", error))?;
            let entry = Module::declare(ctx.clone(), name, prepared.as_bytes())
                .catch(&ctx)
                .map_err(|error| caught_error("declare", error))?;
            let (evaluated, _promise) = entry
                .eval()
                .catch(&ctx)
                .map_err(|error| caught_error("eval", error))?;
            let namespace = evaluated
                .namespace()
                .catch(&ctx)
                .map_err(|error| caught_error("namespace", error))?;
            let default: Option<rquickjs::Value<'_>> = namespace
                .get("default")
                .catch(&ctx)
                .map_err(|error| caught_error("default", error))?;
            if let Some(function) = default.as_ref().and_then(|value| value.as_function()) {
                let api: Object<'_> = ctx
                    .globals()
                    .get("pi")
                    .catch(&ctx)
                    .map_err(|error| caught_error("api", error))?;
                function
                    .call::<_, rquickjs::Value<'_>>((api,))
                    .catch(&ctx)
                    .map_err(|error| caught_error("factory", error))?;
            }
            Ok::<(), PluginError>(())
        })
    }

    /// Load and run a plugin from a file path. Relative imports resolve against
    /// the file, and `.ts`/`.tsx` entries are transpiled.
    pub fn run_file(&self, path: &std::path::Path) -> Result<Vec<HostCall>, PluginError> {
        let source = std::fs::read_to_string(path)
            .map_err(|error| PluginError::Engine(format!("read {}: {error}", path.display())))?;
        self.run_named(&path.to_string_lossy(), &source)
    }

    fn install_pi_global<'js>(&self, ctx: &Ctx<'js>) -> Result<(), PluginError> {
        let pi = Object::new(ctx.clone())?;
        // Registry of registered tools' `execute` functions (name -> function).
        ctx.globals().set("__pi_tools", Object::new(ctx.clone())?)?;

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
        let tools = self.tools.clone();
        pi.set(
            "registerTool",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>, spec: rquickjs::Value<'js>| {
                    let json = json_from_js(&spec);
                    let allowed = record(
                        &policy,
                        &calls,
                        &denials,
                        Capability::Events,
                        "registerTool",
                        json.clone(),
                    );
                    if !allowed {
                        return;
                    }
                    let name = json
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if name.is_empty() {
                        return;
                    }
                    let description = json
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let parameters = json
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
                    // Keep the execute function in a JS-side registry so the
                    // instance can call it after the module has finished loading.
                    if let Some(object) = spec.as_object() {
                        if let Ok(execute) = object.get::<_, rquickjs::Value>("execute") {
                            if execute.is_function() {
                                if let Ok(registry) = ctx.globals().get::<_, Object>("__pi_tools") {
                                    let _ = registry.set(name.clone(), execute);
                                }
                            }
                        }
                    }
                    tools.lock().expect("tools lock").push(PluginToolSpec {
                        name,
                        description,
                        parameters,
                    });
                },
            ),
        )?;

        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        let commands = self.commands.clone();
        pi.set(
            "registerCommand",
            Function::new(
                ctx.clone(),
                move |name: String, spec: rquickjs::Value<'_>| {
                    let json = serde_json::json!({ "name": name, "spec": json_from_js(&spec) });
                    if record(
                        &policy,
                        &calls,
                        &denials,
                        Capability::Events,
                        "registerCommand",
                        json.clone(),
                    ) {
                        commands.lock().expect("commands lock").push(json);
                    }
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

        // appendFileSync(path, data)
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        fs.set(
            "appendFileSync",
            Function::new(ctx.clone(), move |path: String, data: String| {
                if record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Write,
                    "fs.appendFileSync",
                    serde_json::json!({ "path": path }),
                ) {
                    use std::io::Write as _;
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                    {
                        let _ = file.write_all(data.as_bytes());
                    }
                }
            }),
        )?;

        // copyFileSync(src, dest)
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        fs.set(
            "copyFileSync",
            Function::new(ctx.clone(), move |src: String, dest: String| {
                if record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Write,
                    "fs.copyFileSync",
                    serde_json::json!({ "src": src, "dest": dest }),
                ) {
                    let _ = std::fs::copy(&src, &dest);
                }
            }),
        )?;

        // rmSync(path, recursive?)
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        fs.set(
            "rmSync",
            Function::new(ctx.clone(), move |path: String, recursive: Option<bool>| {
                if record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Write,
                    "fs.rmSync",
                    serde_json::json!({ "path": path }),
                ) {
                    let _ = if recursive.unwrap_or(false) {
                        std::fs::remove_dir_all(&path)
                    } else {
                        std::fs::remove_file(&path)
                    };
                }
            }),
        )?;

        // mkdtemp(prefix) -> path
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        fs.set(
            "mkdtemp",
            Function::new(ctx.clone(), move |prefix: String| -> String {
                let allowed = record(
                    &policy,
                    &calls,
                    &denials,
                    Capability::Write,
                    "fs.mkdtemp",
                    serde_json::json!({ "prefix": prefix }),
                );
                if !allowed {
                    return String::new();
                }
                let unique = crate::crypto::random_bytes(6)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                let dir = format!("{prefix}{unique}");
                if std::fs::create_dir_all(&dir).is_ok() {
                    dir
                } else {
                    String::new()
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

    /// Install `__pi_host.zlib` compression helpers.
    fn install_host_zlib(&self, ctx: &Ctx<'_>) -> Result<(), PluginError> {
        let globals = ctx.globals();
        let host: Object = globals.get("__pi_host")?;
        let zlib = Object::new(ctx.clone())?;
        zlib.set(
            "gzipSync",
            Function::new(ctx.clone(), |data: Vec<u8>| -> Vec<u8> { gzip(&data) }),
        )?;
        zlib.set(
            "gunzipSync",
            Function::new(ctx.clone(), |data: Vec<u8>| -> Vec<u8> { gunzip(&data) }),
        )?;
        zlib.set(
            "deflateSync",
            Function::new(ctx.clone(), |data: Vec<u8>| -> Vec<u8> { deflate(&data) }),
        )?;
        zlib.set(
            "inflateSync",
            Function::new(ctx.clone(), |data: Vec<u8>| -> Vec<u8> { inflate(&data) }),
        )?;
        host.set("zlib", zlib)?;
        Ok(())
    }

    /// Install `__pi_host.apiCall`, the catch-all used by the `pi` proxy for
    /// extension methods we have not modeled yet. Records like any hostcall.
    fn install_host_api_call(&self, ctx: &Ctx<'_>) -> Result<(), PluginError> {
        let globals = ctx.globals();
        let host: Object = globals.get("__pi_host")?;
        let (calls, denials, policy) = (
            self.calls.clone(),
            self.denials.clone(),
            self.policy.clone(),
        );
        host.set(
            "apiCall",
            Function::new(ctx.clone(), move |name: String, args_json: String| {
                let args = serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
                record(&policy, &calls, &denials, Capability::Events, &name, args);
            }),
        )?;
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

fn caught_error(stage: &str, error: rquickjs::CaughtError<'_>) -> PluginError {
    match error {
        rquickjs::CaughtError::Exception(exception) => {
            let message = exception.message().unwrap_or_default();
            let stack = exception.stack().unwrap_or_default();
            if stack.is_empty() {
                PluginError::Engine(format!("{stage}: {message}"))
            } else {
                PluginError::Engine(format!("{stage}: {message}\n{stack}"))
            }
        }
        other => PluginError::Engine(format!("{stage}: {other}")),
    }
}

/// gzip-compress bytes.
fn gzip(data: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use std::io::Write as _;
    let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = encoder.write_all(data);
    encoder.finish().unwrap_or_default()
}

/// gzip-decompress bytes.
fn gunzip(data: &[u8]) -> Vec<u8> {
    use flate2::read::GzDecoder;
    use std::io::Read as _;
    let mut out = Vec::new();
    let _ = GzDecoder::new(data).read_to_end(&mut out);
    out
}

/// zlib-deflate bytes.
fn deflate(data: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use std::io::Write as _;
    let mut encoder = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = encoder.write_all(data);
    encoder.finish().unwrap_or_default()
}

/// zlib-inflate bytes.
fn inflate(data: &[u8]) -> Vec<u8> {
    use flate2::read::ZlibDecoder;
    use std::io::Read as _;
    let mut out = Vec::new();
    let _ = ZlibDecoder::new(data).read_to_end(&mut out);
    out
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

/// A loaded extension kept alive so its registered tools and commands can be
/// used after the module has finished evaluating.
pub struct PluginInstance {
    context: Context,
    tools: Vec<PluginToolSpec>,
    commands: Vec<serde_json::Value>,
}

impl PluginInstance {
    pub fn load(policy: PluginPolicy, name: &str, source: &str) -> Result<Self, PluginError> {
        let prepared = if pi_transpile::needs_transpile(name) {
            pi_transpile::transpile(name, source).map_err(PluginError::Engine)?
        } else {
            source.to_string()
        };
        let host = PluginHost::new(policy);
        let runtime = Runtime::new().map_err(PluginError::from)?;
        runtime.set_loader(modules::PiResolver, modules::PiLoader);
        let context = Context::full(&runtime).map_err(PluginError::from)?;
        host.evaluate(&context, name, &prepared)?;
        if let Some(denied) = host.denials.lock().expect("denials lock").first().cloned() {
            return Err(PluginError::Denied {
                capability: denied.capability,
                method: denied.method,
            });
        }
        let tools = host.tools.lock().expect("tools lock").clone();
        let commands = host.commands.lock().expect("commands lock").clone();
        Ok(Self {
            context,
            tools,
            commands,
        })
    }

    pub fn from_file(policy: PluginPolicy, path: &std::path::Path) -> Result<Self, PluginError> {
        let source = std::fs::read_to_string(path)
            .map_err(|error| PluginError::Engine(format!("read {}: {error}", path.display())))?;
        Self::load(policy, &path.to_string_lossy(), &source)
    }

    pub fn tools(&self) -> &[PluginToolSpec] {
        &self.tools
    }

    pub fn commands(&self) -> &[serde_json::Value] {
        &self.commands
    }

    /// Call a registered tool's `execute(input)` and return its JSON result.
    pub fn call_tool(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        self.context.with(|ctx| {
            let registry: Object<'_> =
                ctx.globals().get("__pi_tools").map_err(PluginError::from)?;
            let value: rquickjs::Value<'_> = registry.get(name).map_err(PluginError::from)?;
            let Some(function) = value.as_function() else {
                return Err(PluginError::Engine(format!(
                    "plugin tool `{name}` is not callable"
                )));
            };
            let input = json_to_js(&ctx, input);
            let result: rquickjs::Value<'_> = function
                .call((input,))
                .catch(&ctx)
                .map_err(|error| caught_error("tool", error))?;
            Ok(js_to_json(&result, 0))
        })
    }
}

fn json_to_js<'js>(ctx: &Ctx<'js>, value: &serde_json::Value) -> rquickjs::Value<'js> {
    match value {
        serde_json::Value::Null => rquickjs::Value::new_null(ctx.clone()),
        serde_json::Value::Bool(flag) => rquickjs::Value::new_bool(ctx.clone(), *flag),
        serde_json::Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                rquickjs::Value::new_int(ctx.clone(), int as i32)
            } else {
                rquickjs::Value::new_float(ctx.clone(), number.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(text) => rquickjs::String::from_str(ctx.clone(), text)
            .map(|value| value.into_value())
            .unwrap_or_else(|_| rquickjs::Value::new_null(ctx.clone())),
        serde_json::Value::Array(items) => {
            let array = rquickjs::Array::new(ctx.clone()).expect("array");
            for (index, item) in items.iter().enumerate() {
                let _ = array.set(index, json_to_js(ctx, item));
            }
            array.into_value()
        }
        serde_json::Value::Object(map) => {
            let object = Object::new(ctx.clone()).expect("object");
            for (key, item) in map {
                let _ = object.set(key.as_str(), json_to_js(ctx, item));
            }
            object.into_value()
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
