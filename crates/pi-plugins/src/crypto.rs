//! Hash and random primitives for the virtual `node:crypto` module.
//!
//! Kept dependency-light and pure so the hostcall layer stays thin. Hashes are
//! returned as raw bytes; the JS `node:crypto` module encodes them.

use hmac::{Hmac, Mac};
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

/// Hash `data` with `algo`, returning raw bytes. `None` for unknown algorithms.
pub fn hash_bytes(algo: &str, data: &[u8]) -> Option<Vec<u8>> {
    Some(match algo.to_ascii_lowercase().as_str() {
        "sha256" | "sha-256" => Sha256::digest(data).to_vec(),
        "sha512" | "sha-512" => Sha512::digest(data).to_vec(),
        "sha1" | "sha-1" => Sha1::digest(data).to_vec(),
        "md5" => Md5::digest(data).to_vec(),
        _ => return None,
    })
}

/// HMAC of `data` with `key` under `algo`, as raw bytes.
pub fn hmac_bytes(algo: &str, key: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    macro_rules! mac {
        ($hash:ty) => {{
            let mut mac = Hmac::<$hash>::new_from_slice(key).ok()?;
            mac.update(data);
            Some(mac.finalize().into_bytes().to_vec())
        }};
    }
    match algo.to_ascii_lowercase().as_str() {
        "sha256" | "sha-256" => mac!(Sha256),
        "sha512" | "sha-512" => mac!(Sha512),
        "sha1" | "sha-1" => mac!(Sha1),
        _ => None,
    }
}

/// Cryptographically secure random bytes.
pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    getrandom::getrandom(&mut buf).expect("OS randomness");
    buf
}

/// A random RFC 4122 version 4 UUID.
pub fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS randomness");
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10xx
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8],
        bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

#[cfg(test)]
#[path = "../tests/unit/crypto.rs"]
mod tests;
