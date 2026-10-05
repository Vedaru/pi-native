//! Global prelude injected into every plugin context.
//!
//! Provides the Node globals extensions commonly assume: `Buffer` (a typed-array
//! subclass with utf8/hex/base64/latin1 codecs), `globalThis`, and `process`.

/// JavaScript evaluated before a plugin module. Kept dependency-free.
pub const PRELUDE: &str = r#"
(function () {
  const HEX = "0123456789abcdef";
  const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

  function utf8Encode(str) {
    const out = [];
    for (const ch of String(str)) {
      const cp = ch.codePointAt(0);
      if (cp < 0x80) out.push(cp);
      else if (cp < 0x800) out.push(0xc0 | (cp >> 6), 0x80 | (cp & 0x3f));
      else if (cp < 0x10000) out.push(0xe0 | (cp >> 12), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
      else out.push(0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3f), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
    }
    return out;
  }
  function utf8Decode(bytes) {
    let out = "";
    for (let i = 0; i < bytes.length;) {
      const b = bytes[i];
      if (b < 0x80) { out += String.fromCharCode(b); i += 1; }
      else if (b < 0xe0) { out += String.fromCharCode(((b & 0x1f) << 6) | (bytes[i + 1] & 0x3f)); i += 2; }
      else if (b < 0xf0) { out += String.fromCharCode(((b & 0x0f) << 12) | ((bytes[i + 1] & 0x3f) << 6) | (bytes[i + 2] & 0x3f)); i += 3; }
      else {
        const cp = ((b & 0x07) << 18) | ((bytes[i + 1] & 0x3f) << 12) | ((bytes[i + 2] & 0x3f) << 6) | (bytes[i + 3] & 0x3f);
        out += String.fromCodePoint(cp); i += 4;
      }
    }
    return out;
  }
  function fromBase64(str) {
    const clean = String(str).replace(/[^A-Za-z0-9+/]/g, "");
    const out = [];
    for (let i = 0; i < clean.length; i += 4) {
      const c0 = B64.indexOf(clean[i]);
      const c1 = B64.indexOf(clean[i + 1]);
      const c2 = B64.indexOf(clean[i + 2]);
      const c3 = B64.indexOf(clean[i + 3]);
      out.push((c0 << 2) | (c1 >> 4));
      if (c2 >= 0) out.push(((c1 & 15) << 4) | (c2 >> 2));
      if (c3 >= 0) out.push(((c2 & 3) << 6) | c3);
    }
    return out;
  }
  function toHex(bytes) {
    let s = "";
    for (const b of bytes) s += HEX[(b >> 4) & 15] + HEX[b & 15];
    return s;
  }
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
  function toArray(value, encoding) {
    if (typeof value === "string") {
      if (encoding === "hex") {
        const out = [];
        for (let i = 0; i < value.length; i += 2) out.push(parseInt(value.substr(i, 2), 16) || 0);
        return out;
      }
      if (encoding === "base64") return fromBase64(value);
      if (encoding === "latin1" || encoding === "binary") {
        const out = [];
        for (let i = 0; i < value.length; i++) out.push(value.charCodeAt(i) & 0xff);
        return out;
      }
      return utf8Encode(value);
    }
    if (Array.isArray(value)) return value.slice();
    if (value instanceof Uint8Array) return Array.from(value);
    return [];
  }

  class Buffer extends Uint8Array {
    static from(value, encoding) { return new Buffer(toArray(value, encoding)); }
    static alloc(size, fill) {
      const buf = new Buffer(size);
      if (fill !== undefined) {
        if (typeof fill === "number") buf.fill(fill);
        else if (typeof fill === "string") {
          let bytes = toArray(fill, "utf8");
          if (bytes.length === 0) bytes = [0];
          for (let i = 0; i < buf.length; i++) buf[i] = bytes[i % bytes.length];
        }
      }
      return buf;
    }
    static isBuffer(value) { return value instanceof Buffer; }
    static byteLength(value, encoding) { return toArray(value, encoding).length; }
    static concat(list, totalLength) {
      const arrays = list.map((b) => Array.from(b));
      const length = totalLength === undefined ? arrays.reduce((n, a) => n + a.length, 0) : totalLength;
      const out = new Buffer(length);
      let offset = 0;
      for (const a of arrays) { if (offset >= length) break; const take = Math.min(a.length, length - offset); out.set(a.slice(0, take), offset); offset += take; }
      return out;
    }
    toString(encoding) {
      if (!encoding || encoding === "utf8" || encoding === "utf-8") return utf8Decode(this);
      if (encoding === "hex") return toHex(this);
      if (encoding === "base64") return toBase64(this);
      if (encoding === "latin1" || encoding === "binary") return Array.from(this).map((b) => String.fromCharCode(b)).join("");
      return utf8Decode(this);
    }
    slice(start, end) { return new Buffer(Array.from(this).slice(start, end)); }
    subarray(start, end) { return this.slice(start, end); }
  }

  globalThis.Buffer = Buffer;
  globalThis.global = globalThis;

  const env = globalThis.__pi_env || {};
  globalThis.process = {
    platform: env.platform,
    arch: env.arch,
    cwd: () => env.cwd,
    env: env.env || {},
    argv: ["pipelets"],
    version: "v22.0.0",
    versions: { node: "22.0.0" },
    stdout: { write: (s) => { globalThis.__pi_host.apiCall("process.stdout.write", JSON.stringify([String(s)])); return true; }, isTTY: false },
    stderr: { write: (s) => { globalThis.__pi_host.apiCall("process.stderr.write", JSON.stringify([String(s)])); return true; }, isTTY: false },
    exit: () => {},
    on: () => globalThis.process,
    nextTick: (fn, ...args) => queueMicrotask(() => fn(...args)),
  };

  // Wrap `pi` so any extension method we have not modeled records a hostcall
  // instead of throwing "not a function".
  const base = globalThis.pi || {};
  const safeArgs = (args) => {
    try {
      return JSON.stringify(args.map((a) => (typeof a === "function" ? null : a)));
    } catch (error) {
      return "[]";
    }
  };
  globalThis.pi = new Proxy(base, {
    get(target, prop) {
      if (prop in target) return target[prop];
      if (typeof prop === "symbol") return undefined;
      return (...args) => globalThis.__pi_host.apiCall(String(prop), safeArgs(args));
    },
  });
})();
"#;

#[cfg(test)]
#[path = "../tests/unit/globals.rs"]
mod tests;
