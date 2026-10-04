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
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha256_matches_known_vector() {
        // sha256("abc")
        let digest = hash_bytes("sha256", b"abc").unwrap();
        assert_eq!(
            hex(&digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha1_and_md5_known_vectors() {
        assert_eq!(
            hex(&hash_bytes("sha1", b"abc").unwrap()),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&hash_bytes("md5", b"abc").unwrap()),
            "900150983cd24fb0d6963f7d28e17f72"
        );
    }

    #[test]
    fn unknown_algorithm_is_none() {
        assert!(hash_bytes("whirlpool", b"abc").is_none());
    }

    #[test]
    fn hmac_sha256_matches_known_vector() {
        let mac = hmac_bytes(
            "sha256",
            b"key",
            b"The quick brown fox jumps over the lazy dog",
        )
        .unwrap();
        assert_eq!(
            hex(&mac),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn random_bytes_have_requested_length() {
        assert_eq!(random_bytes(16).len(), 16);
        assert_ne!(random_bytes(16), random_bytes(16));
    }

    #[test]
    fn uuid_has_v4_shape() {
        let uuid = random_uuid();
        assert_eq!(uuid.len(), 36);
        let parts: Vec<&str> = uuid.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(uuid.chars().nth(14).unwrap() == '4');
    }
}
