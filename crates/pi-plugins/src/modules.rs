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
            return Module::declare(ctx.clone(), name, stub_module_source(name));
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
        "node:crypto" => Some(CRYPTO_MODULE.to_string()),
        "node:events" => Some(EVENTS_MODULE.to_string()),
        "node:child_process" => Some(CHILD_PROCESS_MODULE.to_string()),
        "node:buffer" => Some(BUFFER_MODULE.to_string()),
        "node:url" => Some(NODE_URL_MODULE.to_string()),
        "node:module" => Some(NODE_MODULE_MODULE.to_string()),
        "node:util" => Some(UTIL_MODULE.to_string()),
        "node:zlib" => Some(ZLIB_MODULE.to_string()),
        "node:http" => Some(HTTP_MODULE.to_string()),
        "node:https" => Some(HTTPS_MODULE.to_string()),
        "node:readline" | "node:readline/promises" => Some(READLINE_MODULE.to_string()),
        _ => None,
    }
}

/// Generic npm/package adapter.
///
/// Any property access resolves to a callable and constructable proxy, so a
/// plugin that imports a package we cannot run still loads and registers. This
/// is deliberately name-agnostic: there are no per-package or per-export
/// special cases. Static ESM named imports are handled by the transpiler's
/// generic import rewrite.
pub fn stub_module_source(spec: &str) -> String {
    let mut source = String::new();
    source.push_str("// pi-native generic package stub for ");
    source.push_str(spec);
    source.push_str(
        "\nconst __stub = new Proxy(function () {}, {\n\
           get: () => __stub,\n\
           apply: () => undefined,\n\
           construct: () => ({}),\n\
         });\n\
         export default __stub;\n",
    );
    source
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
  const env = globalThis.__pi_env || {};
  let resolved = "";
  let absolute = false;
  for (let i = args.length - 1; i >= 0; i--) {
    const arg = args[i];
    if (typeof arg !== "string" || arg === "") continue;
    resolved = arg + (resolved ? "/" + resolved : "");
    if (arg.startsWith("/")) { absolute = true; break; }
  }
  if (!absolute) resolved = (env.cwd || "/") + "/" + resolved;
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
export function appendFileSync(path, data, _options) { host.appendFileSync(path, typeof data === "string" ? data : String(data)); }
export function copyFileSync(src, dest) { host.copyFileSync(src, dest); }
export function rmSync(path, options) { host.rmSync(path, !!(options && options.recursive)); }
export function mkdtempSync(prefix) { return host.mkdtemp(prefix); }
export function createReadStream(_path, _options) {
  return { on() { return this; }, once() { return this; }, pipe() { return this; }, close() {}, destroy() {}, async *[Symbol.asyncIterator]() {} };
}
export const constants = {};
export default { readFileSync, writeFileSync, existsSync, readdirSync, mkdirSync, unlinkSync, appendFileSync, copyFileSync, rmSync, mkdtempSync, createReadStream, constants };
"#;

const FS_PROMISES_MODULE: &str = r#"
const host = globalThis.__pi_host.fs;
const enc = (options) => (typeof options === "string" ? options : (options && options.encoding) || undefined);
export async function readFile(path, options) { return host.readFileSync(path, enc(options)); }
export async function writeFile(path, data, _options) { host.writeFileSync(path, String(data)); }
export async function readdir(path, _options) { return host.readdirSync(path); }
export async function mkdir(path, options) { host.mkdirSync(path, !!(options && options.recursive)); }
export async function unlink(path) { host.unlinkSync(path); }
export async function appendFile(path, data) { host.appendFileSync(path, typeof data === "string" ? data : String(data)); }
export async function rm(path, options) { host.rmSync(path, !!(options && options.recursive)); }
export async function mkdtemp(prefix) { return host.mkdtemp(prefix); }
export async function access(path) { if (!host.existsSync(path)) throw new Error("ENOENT: " + path); }
export default { readFile, writeFile, readdir, mkdir, unlink, appendFile, rm, mkdtemp, access };
"#;

const CRYPTO_MODULE: &str = r#"
const host = globalThis.__pi_host.crypto;
const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
function toHex(bytes) { let s = ""; for (const b of bytes) s += b.toString(16).padStart(2, "0"); return s; }
function toBase64(bytes) {
  let out = "";
  for (let i = 0; i < bytes.length; i += 3) {
    const b0 = bytes[i], b1 = bytes[i + 1], b2 = bytes[i + 2];
    out += B64[b0 >> 2];
    out += B64[((b0 & 3) << 4) | ((b1 || 0) >> 4)];
    out += b1 === undefined ? "=" : B64[((b1 & 15) << 2) | ((b2 || 0) >> 6)];
    out += b2 === undefined ? "=" : B64[b2 & 63];
  }
  return out;
}
function toUtf8(bytes) { return bytes.map((b) => String.fromCharCode(b)).join(""); }
function encode(bytes, encoding) {
  if (!encoding || encoding === "utf8" || encoding === "utf-8") return toUtf8(bytes);
  if (encoding === "hex") return toHex(bytes);
  if (encoding === "base64") return toBase64(bytes);
  return toUtf8(bytes);
}
export class Hash {
  constructor(algo) { this.algo = algo; this.data = ""; }
  update(data) { this.data += String(data); return this; }
  digest(encoding) { return encode(host.hashBytes(this.algo, this.data), encoding); }
}
export class Hmac extends Hash {
  constructor(algo, key) { super(algo); this.key = String(key); }
  digest(encoding) { return encode(host.hmacBytes(this.algo, this.key, this.data), encoding); }
}
export function createHash(algo) { return new Hash(algo); }
export function createHmac(algo, key) { return new Hmac(algo, key); }
export function randomBytes(n) { return host.randomBytes(n); }
export function randomUUID() { return host.randomUUID(); }
export default { createHash, createHmac, randomBytes, randomUUID, Hash, Hmac };
"#;

const EVENTS_MODULE: &str = r#"
export class EventEmitter {
  constructor() { this._events = new Map(); }
  on(name, fn) { const list = this._events.get(name) || []; list.push(fn); this._events.set(name, list); return this; }
  addListener(name, fn) { return this.on(name, fn); }
  prependListener(name, fn) { const list = this._events.get(name) || []; list.unshift(fn); this._events.set(name, list); return this; }
  once(name, fn) { const wrap = (...args) => { this.off(name, wrap); fn(...args); }; return this.on(name, wrap); }
  off(name, fn) { const list = this._events.get(name); if (list) { const i = list.indexOf(fn); if (i >= 0) list.splice(i, 1); } return this; }
  removeListener(name, fn) { return this.off(name, fn); }
  removeAllListeners(name) { if (name === undefined) this._events.clear(); else this._events.delete(name); return this; }
  emit(name, ...args) { const list = this._events.get(name); if (!list || list.length === 0) return false; for (const fn of list.slice()) fn(...args); return true; }
  listenerCount(name) { const list = this._events.get(name); return list ? list.length : 0; }
  listeners(name) { return (this._events.get(name) || []).slice(); }
  eventNames() { return [...this._events.keys()]; }
}
export function once(emitter, name) { return new Promise((resolve) => emitter.once(name, (...args) => resolve(args))); }
export default EventEmitter;
"#;

const CHILD_PROCESS_MODULE: &str = r#"
const host = globalThis.__pi_host.child_process;
export function execSync(command, _options) { return host.execShell(String(command)); }
export function exec(command, options, callback) {
  let cb = callback;
  if (typeof options === "function") { cb = options; }
  queueMicrotask(() => {
    try { const stdout = host.execShell(String(command)); if (cb) cb(null, stdout, ""); }
    catch (error) { if (cb) cb(error, "", ""); }
  });
  return { pid: 0, kill() {} };
}
export function spawnSync(command, args, _options) {
  const result = JSON.parse(host.spawnSync(String(command), (args || []).map(String)));
  return { status: result.status, stdout: result.stdout, stderr: result.stderr, pid: 0 };
}
export function spawn(command, args, _options) {
  const listeners = { stdout: [], stderr: [], exit: [], error: [], close: [] };
  const child = {
    pid: 0,
    stdout: { on: (n, f) => listeners.stdout.push(f), setEncoding() {} },
    stderr: { on: (n, f) => listeners.stderr.push(f), setEncoding() {} },
    on: (n, f) => { (listeners[n] = listeners[n] || []).push(f); return child; },
    once: (n, f) => { (listeners[n] = listeners[n] || []).push(f); return child; },
    kill() {},
  };
  queueMicrotask(() => {
    const result = JSON.parse(host.spawnSync(String(command), (args || []).map(String)));
    if (result.stdout) for (const f of listeners.stdout) f(result.stdout);
    if (result.stderr) for (const f of listeners.stderr) f(result.stderr);
    for (const f of listeners.exit) f(result.status, null);
    for (const f of listeners.close) f(result.status, null);
  });
  return child;
}
export function execFileSync(file, args, _options) {
  return host.execShell([file].concat(args || []).join(" "));
}
export default { exec, execSync, spawn, spawnSync, execFileSync };
"#;

const BUFFER_MODULE: &str = r#"
const Buffer = globalThis.Buffer;
export { Buffer };
export default { Buffer };
"#;

const NODE_URL_MODULE: &str = r##"
export function pathToFileURL(path) {
  const value = String(path);
  return { href: "file://" + (value.startsWith("/") ? "" : "/") + value, pathname: value };
}
export function fileURLToPath(url) {
  const value = typeof url === "string" ? url : (url && url.href) || "";
  return decodeURIComponent(value.replace(/^file:\/\//, ""));
}
export class URL {
  constructor(input, base) {
    const value = String(input);
    this.href = /^[a-z]+:\/\//i.test(value) || !base ? value : String(base).replace(/\/$/, "") + "/" + value;
    const noScheme = this.href.replace(/^[a-z]+:\/\//i, "");
    const slash = noScheme.indexOf("/");
    this.pathname = slash === -1 ? "" : noScheme.slice(slash);
    this.searchParams = new URLSearchParams((this.href.split("?")[1] || "").split("#")[0]);
  }
  toString() { return this.href; }
}
class URLSearchParams {
  constructor(init) { this._p = new Map(); if (typeof init === "string" && init) for (const pair of init.split("&")) { const [k, v] = pair.split("="); if (k) this._p.set(decodeURIComponent(k), decodeURIComponent(v || "")); } }
  get(k) { return this._p.has(k) ? this._p.get(k) : null; }
  set(k, v) { this._p.set(k, String(v)); }
  has(k) { return this._p.has(k); }
}
export { URLSearchParams };
export default { pathToFileURL, fileURLToPath, URL, URLSearchParams };
"##;

const NODE_MODULE_MODULE: &str = r#"
export function createRequire() {
  const require = (specifier) => { throw new Error("createRequire is not supported in the plugin sandbox: " + specifier); };
  require.resolve = (specifier) => specifier;
  return require;
}
export default { createRequire };
"#;

const UTIL_MODULE: &str = r#"
export function format(...args) {
  if (args.length === 0) return "";
  const first = args[0];
  if (typeof first !== "string") return args.map((a) => inspect(a)).join(" ");
  let i = 1;
  const text = first.replace(/%[sdifjoO%]/g, (token) => {
    if (token === "%%") return "%";
    if (i >= args.length) return token;
    const value = args[i++];
    return typeof value === "string" ? value : inspect(value);
  });
  return [text, ...args.slice(i).map((a) => inspect(a))].join(" ");
}
export function inspect(value) {
  try { return typeof value === "string" ? value : JSON.stringify(value); } catch (error) { return String(value); }
}
export function promisify(fn) {
  return (...args) => new Promise((resolve, reject) => {
    fn(...args, (error, value) => (error ? reject(error) : resolve(value)));
  });
}
export function inherits(ctor, superCtor) {
  Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
  Object.setPrototypeOf(ctor, superCtor);
}
export function deprecate(fn) { return fn; }
export const types = { isDate: (v) => v instanceof Date, isRegExp: (v) => v instanceof RegExp, isArray: Array.isArray };
export default { format, inspect, promisify, inherits, deprecate, types };
"#;

const READLINE_MODULE: &str = r#"
export function createInterface() {
  return {
    on() { return this; },
    once() { return this; },
    close() {},
    question(_query, callback) { if (callback) queueMicrotask(() => callback("")); },
    write() {},
    prompt() {},
    async *[Symbol.asyncIterator]() {},
  };
}
export default { createInterface };
"#;

const ZLIB_MODULE: &str = r#"
const host = globalThis.__pi_host.zlib;
function toBytes(data) {
  if (data instanceof Uint8Array) return Array.from(data);
  if (typeof data === "string") return Array.from(Buffer.from(data));
  return Array.from(data || []);
}
export function gzipSync(data) { return Buffer.from(host.gzipSync(toBytes(data))); }
export function gunzipSync(data) { return Buffer.from(host.gunzipSync(toBytes(data))); }
export function deflateSync(data) { return Buffer.from(host.deflateSync(toBytes(data))); }
export function inflateSync(data) { return Buffer.from(host.inflateSync(toBytes(data))); }
export const constants = {};
export default { gzipSync, gunzipSync, deflateSync, inflateSync, constants };
"#;

// The resolver canonicalizes `node:http`/`node:https`, so they must have a
// module body; otherwise an import fails at load. The native host has no HTTP
// capability yet, so the calls throw with a clear message instead of a
// confusing "could not read" loader error.
const HTTP_UNSUPPORTED: &str = r#"
function unsupported() { throw new Error("node:http is not supported by the native plugin host"); }
export function request(..._args) { return unsupported(); }
export function get(..._args) { return unsupported(); }
export function createServer(..._args) { return unsupported(); }
export class Agent { constructor() { unsupported(); } }
export const STATUS_CODES = {};
export default { request, get, createServer, Agent, STATUS_CODES };
"#;

const HTTP_MODULE: &str = HTTP_UNSUPPORTED;

const HTTPS_MODULE: &str = r#"
function unsupported() { throw new Error("node:https is not supported by the native plugin host"); }
export function request(..._args) { return unsupported(); }
export function get(..._args) { return unsupported(); }
export function createServer(..._args) { return unsupported(); }
export class Agent { constructor() { unsupported(); } }
export const globalAgent = {};
export default { request, get, createServer, Agent, globalAgent };
"#;

#[cfg(test)]
#[path = "../tests/unit/modules.rs"]
mod tests;
