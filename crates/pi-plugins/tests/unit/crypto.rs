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
