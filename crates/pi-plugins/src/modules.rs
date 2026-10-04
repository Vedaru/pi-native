//! Module resolution and virtual modules for the QuickJS plugin host.
//!
//! Mirrors the reference port's model:
//! - Node built-ins resolve to Rust-provided virtual modules (JS implemented in
//!   terms of a small injected `__pi_env` plus, later, hostcalls).
//! - Bare npm specifiers resolve to a proxy stub so a plugin loads and registers
//!   even though the library behavior is absent.
//! - Relative/absolute paths resolve against the importing module and load from
//!   disk (JavaScript only at this stage; `swc` transpilation lands next).

use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::{Ctx, Module, Result as QjsResult};

/// Canonicalize a Node built-in specifier (with or without the `node:` prefix).
pub fn canonical_node_builtin(spec: &str) -> Option<&'static str> {
    Some(match spec {
        "fs" | "node:fs" => "node:fs",
        "fs/promises" | "node:fs/promises" => "node:fs/promises",
        "path" | "node:path" => "node:path",
        "os" | "node:os" => "node:os",
        "child_process" | "node:child_process" => "node:child_process",
        "crypto" | "node:crypto" => "node:crypto",
        "http" | "node:http" => "node:http",
        "https" | "node:https" => "node:https",
        "stream" | "node:stream" => "node:stream",
        "buffer" | "node:buffer" => "node:buffer",
        "events" | "node:events" => "node:events",
        "url" | "node:url" => "node:url",
        "util" | "node:util" => "node:util",
        "assert" | "node:assert" => "node:assert",
        "module" | "node:module" => "node:module",
        "timers" | "node:timers" => "node:timers",
        "process" | "node:process" => "node:process",
        _ => return None,
    })
}

fn is_relative(spec: &str) -> bool {
    spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/')
}

fn is_bare(spec: &str) -> bool {
    !is_relative(spec) && !spec.starts_with("file:") && !spec.starts_with("node:")
}

/// Resolver that canonicalizes built-ins and passes paths through.
#[derive(Debug, Default)]
pub struct PiResolver;

impl Resolver for PiResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> QjsResult<String> {
        if let Some(canonical) = canonical_node_builtin(name) {
            return Ok(canonical.to_string());
        }
        if is_relative(name) {
            return Ok(resolve_relative(base, name));
        }
        // Bare specifiers are handled by the loader as stubs.
        Ok(name.to_string())
    }
}

fn resolve_relative(base: &str, name: &str) -> String {
    let dir = match base.rfind('/') {
        Some(index) => &base[..index],
        None => "",
    };
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for segment in name.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Loader that serves virtual modules, stubs, and (simple) file modules.
#[derive(Debug, Default)]
pub struct PiLoader;

impl Loader for PiLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> QjsResult<Module<'js, Declared>> {
        if let Some(source) = virtual_module_source(name) {
            return Module::declare(ctx.clone(), name, source);
        }
        if is_bare(name) {
            return Module::declare(ctx.clone(), name, proxy_stub_source(name));
        }
        // Relative/absolute file import.
        let path = name.strip_prefix("file://").unwrap_or(name);
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let source = if pi_transpile::needs_transpile(path) {
                    pi_transpile::transpile(path, &raw)
                        .map_err(|err| rquickjs::Error::new_loading_message(name, err))?
                } else {
                    raw
                };
                Module::declare(ctx.clone(), name, source)
            }
            Err(error) => Err(rquickjs::Error::new_loading_message(
                name,
                format!("could not read {path}: {error}"),
            )),
        }
    }
}

/// JS source for a known virtual module, or `None`.
pub fn virtual_module_source(canonical: &str) -> Option<String> {
    match canonical {
        "node:path" => Some(PATH_MODULE.to_string()),
        "node:os" => Some(OS_MODULE.to_string()),
        "node:process" => Some(PROCESS_MODULE.to_string()),
        "node:fs" => Some(FS_MODULE.to_string()),
        "node:fs/promises" => Some(FS_PROMISES_MODULE.to_string()),
        _ => None,
    }
}

/// A proxy stub for npm packages that cannot run in the sandbox.
fn proxy_stub_source(spec: &str) -> String {
    format!(
        "// pi-native npm proxy stub for {spec}: loads, but has no behavior.\n\
         const stub = new Proxy(function () {{}}, {{\n\
           get: () => stub,\n\
           apply: () => undefined,\n\
           construct: () => ({{}}),\n\
         }});\n\
         export default stub;\n"
    )
}

const PATH_MODULE: &str = r#"
function normalize(parts) {
  const out = [];
  for (const part of parts) {
    if (part === "" || part === ".") continue;
    if (part === "..") { if (out.length && out[out.length-1] !== "..") out.pop(); else out.push(".."); }
    else out.push(part);
  }
  return out;
}
function join(...args) {
  const joined = args.filter((a) => a && a.length).join("/");
  const absolute = joined.startsWith("/") || args.length && args[0].startsWith("/");
  const parts = normalize(joined.split("/"));
  const result = parts.join("/");
  return absolute ? "/" + result : (result || ".");
}
function normalizePath(p) {
  if (!p) return ".";
  const absolute = p.startsWith("/");
  const trailing = p.endsWith("/");
  let parts = normalize(p.split("/"));
  if (parts.length === 0) return absolute ? "/" : trailing ? "./" : ".";
  let result = parts.join("/");
  if (absolute) result = "/" + result;
  if (trailing && !result.endsWith("/")) result += "/";
  return result;
}
function dirname(p) {
  if (!p) return ".";
  const absolute = p.startsWith("/");
  const trimmed = p.replace(/\/+$/, "");
  const index = trimmed.lastIndexOf("/");
  if (index === -1) return absolute ? "/" : ".";
  if (index === 0) return "/";
  return trimmed.slice(0, index);
}
function basename(p, ext) {
  const trimmed = p.replace(/\/+$/, "");
  const index = trimmed.lastIndexOf("/");
  let base = index === -1 ? trimmed : trimmed.slice(index + 1);
  if (ext && base.endsWith(ext)) base = base.slice(0, -ext.length);
  return base;
}
function extname(p) {
  const base = basename(p);
  const index = base.lastIndexOf(".");
  return index <= 0 ? "" : base.slice(index);
}
function isAbsolute(p) { return p.startsWith("/"); }
function resolve(...args) {
  let resolved = "";
  for (let i = args.length - 1; i >= 0 && !resolved.startsWith("/"); i--) {
    const arg = args[i];
    if (!arg) continue;
    resolved = arg + (resolved ? "/" + resolved : "");
  }
  if (!resolved.startsWith("/")) resolved = "/__cwd__/" + resolved;
  return normalizePath(resolved);
}
function relative(from, to) {
  const fromParts = normalizePath(resolve(from)).split("/").filter(Boolean);
  const toParts = normalizePath(resolve(to)).split("/").filter(Boolean);
  let i = 0;
  while (i < fromParts.length && i < toParts.length && fromParts[i] === toParts[i]) i++;
  const up = new Array(fromParts.length - i).fill("..");
  return up.concat(toParts.slice(i)).join("/");
}
const sep = "/";
const delimiter = ":";
const posix = { normalize: normalizePath, join, dirname, basename, extname, isAbsolute, resolve, relative, sep, delimiter };
export { normalizePath as normalize, join, dirname, basename, extname, isAbsolute, resolve, relative, sep, delimiter };
export default posix;
"#;

const OS_MODULE: &str = r#"
const env = globalThis.__pi_env || {};
export function platform() { return env.platform; }
export function arch() { return env.arch; }
export function homedir() { return env.homedir; }
export function tmpdir() { return env.tmpdir; }
export function type() { return env.platform === "win32" ? "Windows_NT" : "Linux"; }
export const EOL = env.eol;
export default { platform, arch, homedir, tmpdir, type, EOL };
"#;

const PROCESS_MODULE: &str = r#"
const env = globalThis.__pi_env || {};
export const platform = env.platform;
export const arch = env.arch;
export const env2 = env.env || {};
export const cwd = () => env.cwd;
export default { platform, arch, cwd, env: env.env || {} };
"#;

const FS_MODULE: &str = r#"
const host = globalThis.__pi_host.fs;
export function readFileSync(path, options) {
  const encoding = typeof options === "string" ? options : (options && options.encoding) || undefined;
  return host.readFileSync(path, encoding);
}
export function writeFileSync(path, data, _options) {
  const text = typeof data === "string" ? data : String(data);
  host.writeFileSync(path, text);
}
export function existsSync(path) { return host.existsSync(path); }
export function readdirSync(path, _options) { return host.readdirSync(path); }
export function mkdirSync(path, options) {
  host.mkdirSync(path, !!(options && options.recursive));
}
export function unlinkSync(path) { host.unlinkSync(path); }
export const constants = {};
export default { readFileSync, writeFileSync, existsSync, readdirSync, mkdirSync, unlinkSync, constants };
"#;

const FS_PROMISES_MODULE: &str = r#"
const host = globalThis.__pi_host.fs;
const enc = (options) => (typeof options === "string" ? options : (options && options.encoding) || undefined);
export async function readFile(path, options) { return host.readFileSync(path, enc(options)); }
export async function writeFile(path, data, _options) { host.writeFileSync(path, String(data)); }
export async function readdir(path, _options) { return host.readdirSync(path); }
export async function mkdir(path, options) { host.mkdirSync(path, !!(options && options.recursive)); }
export async function unlink(path) { host.unlinkSync(path); }
export async function access(path) { if (!host.existsSync(path)) throw new Error("ENOENT: " + path); }
export default { readFile, writeFile, readdir, mkdir, unlink, access };
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_node_builtins() {
        assert_eq!(canonical_node_builtin("fs"), Some("node:fs"));
        assert_eq!(canonical_node_builtin("node:path"), Some("node:path"));
        assert_eq!(canonical_node_builtin("three"), None);
    }

    #[test]
    fn resolves_relative_paths() {
        assert_eq!(
            resolve_relative("src/plugin.ts", "./util.ts"),
            "src/util.ts"
        );
        assert_eq!(resolve_relative("src/plugin.ts", "../lib/x.js"), "lib/x.js");
    }

    #[test]
    fn provides_path_os_fs_and_process_virtual_modules() {
        assert!(virtual_module_source("node:path").is_some());
        assert!(virtual_module_source("node:os").is_some());
        assert!(virtual_module_source("node:process").is_some());
        assert!(virtual_module_source("node:fs").is_some());
        assert!(virtual_module_source("node:fs/promises").is_some());
        assert!(virtual_module_source("node:crypto").is_none());
    }

    #[test]
    fn bare_specifier_gets_a_proxy_stub() {
        let source = proxy_stub_source("some-npm-pkg");
        assert!(source.contains("export default stub"));
    }
}
