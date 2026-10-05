//! Shared measurement helpers for the gates.
//!
//! Every primitive here mirrors what the Python harnesses used: RSS from
//! `/proc/<pid>/status` (`VmRSS`) or `/proc/<pid>/stat` (`VmHWM` for a peak),
//! CPU seconds from `/proc/<pid>/stat`, and wall clock from `Instant`.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The workspace root: the parent of the crate's `CARGO_MANIFEST_DIR`
/// (`crates/pi-gate`). This is the same root the Python scripts derived from
/// `Path(__file__).resolve().parents[1]`.
pub fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("pi-gate lives at <root>/crates/pi-gate")
        .to_path_buf()
}

/// Current RSS of a live process in bytes, or `None` if it exited or is
/// unreadable. Linux reads `/proc/<pid>/status` `VmRSS`; other platforms fall
/// back to `ps -o rss=`.
pub fn rss_bytes(pid: u32) -> Option<u64> {
    if cfg!(target_os = "linux") {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        return status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(parse_kb);
    }
    // macOS / BSD fallback.
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let kb: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (kb > 0).then_some(kb * 1024)
}

/// CPU time (user + system) of a live process in seconds, from
/// `/proc/<pid>/stat`. The comm field can contain spaces and parentheses, so
/// fields are counted from after the last `)`: index 11 is utime, 12 stime,
/// each in clock ticks (100/s on Linux). Shared primitive; the swarm gate uses
/// it to observe that a busy unit is actually consuming CPU.
pub fn cpu_seconds(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) / 100.0)
}

/// Parse a `/proc` status value like `VmRSS:  4096 kB` into bytes. Expects the
/// `"  4096 kB"` remainder after the key.
fn parse_kb(rest: &str) -> Option<u64> {
    let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
    Some(kb * 1024)
}

/// Median of integer samples. Even counts average the two middle values, and
/// the result is truncated to an integer, matching the Python `median`.
pub fn median(values: &[u64]) -> u64 {
    let mut ordered = values.to_vec();
    ordered.sort_unstable();
    let n = ordered.len();
    if n % 2 == 1 {
        ordered[n / 2]
    } else {
        (ordered[n / 2 - 1] + ordered[n / 2]) / 2
    }
}

/// Human-readable MB to one decimal, like the Python `human_mb`.
pub fn human_mb(value: u64) -> String {
    format!("{:.1} MB", value as f64 / 1_048_576.0)
}

/// Human-readable MB to one decimal, or `-` for a missing value.
pub fn human_mb_opt(value: Option<u64>) -> String {
    match value {
        Some(value) => human_mb(value),
        None => "-".to_string(),
    }
}

/// SHA-256 of a file, or `None` if it cannot be read. Used to record which
/// binary a memory sample came from.
pub fn sha256_file(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    // SHA-256 implemented here (no extra dependency) to keep the gate tiny.
    let mut state = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        state.update(&buffer[..read]);
    }
    Some(state.finish_hex())
}

/// Minimal SHA-256 (FIPS 180-4). The gate hashes a few binaries per run, so a
/// small in-tree implementation avoids pulling a crypto crate into CI tooling.
struct Sha256 {
    state: [u32; 8],
    buffer: Vec<u8>,
    length: u64,
}

impl Sha256 {
    fn new() -> Self {
        Sha256 {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: Vec::new(),
            length: 0,
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.length += data.len() as u64;
        self.buffer.extend_from_slice(data);
        while self.buffer.len() >= 64 {
            let block: [u8; 64] = self.buffer[..64].try_into().expect("64-byte block");
            self.compress(&block);
            self.buffer.drain(..64);
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut w = [0u32; 64];
        for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut h = self.state;
        for i in 0..64 {
            let s1 = h[4].rotate_right(6) ^ h[4].rotate_right(11) ^ h[4].rotate_right(25);
            let ch = (h[4] & h[5]) ^ (!h[4] & h[6]);
            let temp1 = h[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = h[0].rotate_right(2) ^ h[0].rotate_right(13) ^ h[0].rotate_right(22);
            let maj = (h[0] & h[1]) ^ (h[0] & h[2]) ^ (h[1] & h[2]);
            let temp2 = s0.wrapping_add(maj);
            h[7] = h[6];
            h[6] = h[5];
            h[5] = h[4];
            h[4] = h[3].wrapping_add(temp1);
            h[3] = h[2];
            h[2] = h[1];
            h[1] = h[0];
            h[0] = temp1.wrapping_add(temp2);
        }
        for (slot, value) in self.state.iter_mut().zip(h) {
            *slot = slot.wrapping_add(value);
        }
    }

    fn finish_hex(mut self) -> String {
        let bit_length = self.length * 8;
        self.buffer.push(0x80);
        while self.buffer.len() % 64 != 56 {
            self.buffer.push(0);
        }
        self.buffer.extend_from_slice(&bit_length.to_be_bytes());
        let blocks: Vec<[u8; 64]> = self.buffer.as_chunks::<64>().0.to_vec();
        for block in &blocks {
            self.compress(block);
        }
        let mut out = String::with_capacity(64);
        for word in self.state {
            out.push_str(&format!("{word:08x}"));
        }
        out
    }
}

/// Sleep for a duration, saturating at zero.
pub fn sleep(seconds: f64) {
    if seconds > 0.0 {
        std::thread::sleep(Duration::from_secs_f64(seconds));
    }
}

/// Current UTC time as an RFC 3339 string, like Python's
/// `datetime.now(timezone.utc).isoformat()` (`+00:00` suffix).
pub fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let micros = now.subsec_micros();
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = time / 3600;
    let minute = (time % 3600) / 60;
    let second = time % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}+00:00")
}

/// Convert days since the Unix epoch to a civil (year, month, day). Howard
/// Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        let empty = Sha256::new();
        assert_eq!(
            empty.finish_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let mut abc = Sha256::new();
        abc.update(b"abc");
        assert_eq!(
            abc.finish_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Longer than one block, to exercise the block loop and padding.
        let mut long = Sha256::new();
        long.update(&vec![b'a'; 1_000_000]);
        assert_eq!(
            long.finish_hex(),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn median_matches_python_semantics() {
        assert_eq!(median(&[5]), 5);
        assert_eq!(median(&[3, 1, 2]), 2);
        assert_eq!(median(&[4, 1, 3, 2]), 2); // (2 + 3) / 2 truncated
    }

    #[test]
    fn rfc3339_is_well_formed() {
        let text = now_rfc3339();
        assert!(text.ends_with("+00:00"), "{text}");
        assert_eq!(&text[4..5], "-");
        assert_eq!(&text[10..11], "T");
    }
}
