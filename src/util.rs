//! Small helpers shared by all programs.

use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A random URL-safe token with `bytes` bytes of entropy (hex encoded).
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    hex::encode(buf)
}

/// Lowercase hex SHA-256 of some bytes.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Compare two secrets without leaking timing information.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Turn the server's public `http(s)://host[:port]` address into the matching
/// `ws(s)://` address with the given path.
pub fn ws_url(server: &str, path: &str) -> String {
    let base = server.trim().trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if base.starts_with("ws://") || base.starts_with("wss://") {
        base.to_string()
    } else {
        format!("wss://{base}")
    };
    format!("{base}{path}")
}

/// Normalise what a person types as a server address.
pub fn normalize_server_url(input: &str) -> String {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        return String::new();
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("https://{s}")
    }
}

/// Minimal percent-encoding for query values.
pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (small pictures inside JSON).
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decode [`base64_encode`]'s output. `None` for anything malformed.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=').as_bytes();
    if s.len() % 4 == 1 {
        return None;
    }
    let val = |b: u8| B64.iter().position(|&x| x == b).map(|v| v as u32);
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.chunks(4) {
        let mut n = 0u32;
        for (i, &b) in c.iter().enumerate() {
            n |= val(b)? << (18 - 6 * i);
        }
        out.extend_from_slice(&n.to_be_bytes()[1..c.len()]);
    }
    Some(out)
}

/// Human friendly "in 2 h 5 min" / "3 min ago" text.
pub fn human_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else if secs < 86_400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 { format!("{h} h") } else { format!("{h} h {m} min") }
    } else {
        format!("{} d", secs / 86_400)
    }
}

/// A unique temporary directory (used by tests and by the updater).
pub fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", random_token(6)));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The Rust target triple this binary was built for, used to pick update files.
pub fn current_target() -> &'static str {
    if cfg!(all(windows, target_arch = "x86_64")) {
        "x86_64-pc-windows-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

/// File name of a program on the current platform (adds `.exe` on Windows).
pub fn exe_name(base: &str, target: &str) -> String {
    if target.contains("windows") { format!("{base}.exe") } else { base.to_string() }
}

/// Operating system label for the machine list.
pub fn os_label() -> String {
    if cfg!(windows) {
        "Windows".into()
    } else if cfg!(target_os = "linux") {
        let pretty = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
            });
        pretty.unwrap_or_else(|| "Linux".into())
    } else {
        std::env::consts::OS.into()
    }
}

/// This computer's name.
pub fn hostname() -> String {
    if let Ok(h) = std::env::var("COMPUTERNAME") {
        return h;
    }
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "computer".into())
}

/// Tiny command-line parser: `cmd --key value --switch positional`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Args {
    pub positional: Vec<String>,
    pub options: std::collections::BTreeMap<String, String>,
    pub switches: std::collections::BTreeSet<String>,
}

impl Args {
    /// `takes_value` lists the options followed by a value.
    pub fn parse(args: impl IntoIterator<Item = String>, takes_value: &[&str]) -> Args {
        let mut out = Args::default();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            if let Some(name) = a.strip_prefix("--") {
                if let Some((k, v)) = name.split_once('=') {
                    out.options.insert(k.to_string(), v.to_string());
                } else if takes_value.contains(&name) {
                    if let Some(v) = it.next() {
                        out.options.insert(name.to_string(), v);
                    }
                } else {
                    out.switches.insert(name.to_string());
                }
            } else {
                out.positional.push(a);
            }
        }
        out
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.options.get(k).map(String::as_str)
    }

    pub fn has(&self, k: &str) -> bool {
        self.switches.contains(k)
    }

    pub fn command(&self) -> Option<&str> {
        self.positional.first().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_parsing() {
        let a = Args::parse(
            ["install", "--server", "https://x", "--dry-run", "--token=abc", "extra"].map(String::from),
            &["server", "token"],
        );
        assert_eq!(a.command(), Some("install"));
        assert_eq!(a.get("server"), Some("https://x"));
        assert_eq!(a.get("token"), Some("abc"));
        assert!(a.has("dry-run"));
        assert_eq!(a.positional, vec!["install", "extra"]);
    }

    #[test]
    fn ws_url_maps_schemes() {
        assert_eq!(ws_url("https://rd.example.com/", "/ws/agent"), "wss://rd.example.com/ws/agent");
        assert_eq!(ws_url("http://10.0.0.5:8080", "/x"), "ws://10.0.0.5:8080/x");
        assert_eq!(ws_url("rd.example.com", "/x"), "wss://rd.example.com/x");
    }

    #[test]
    fn normalize_adds_https() {
        assert_eq!(normalize_server_url(" rd.example.com/ "), "https://rd.example.com");
        assert_eq!(normalize_server_url("http://1.2.3.4:8080"), "http://1.2.3.4:8080");
        assert_eq!(normalize_server_url(""), "");
    }

    #[test]
    fn tokens_are_random_and_hex() {
        let a = random_token(16);
        let b = random_token(16);
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn durations_read_well() {
        assert_eq!(human_duration(30), "30 s");
        assert_eq!(human_duration(600), "10 min");
        assert_eq!(human_duration(7200), "2 h");
        assert_eq!(human_duration(7500), "2 h 5 min");
        assert_eq!(human_duration(200_000), "2 d");
    }

    #[test]
    fn url_encoding() {
        assert_eq!(url_encode("a b&c"), "a%20b%26c");
    }

    #[test]
    fn exe_names() {
        assert_eq!(exe_name("farlink-host", "x86_64-pc-windows-gnu"), "farlink-host.exe");
        assert_eq!(exe_name("farlink-host", "x86_64-unknown-linux-gnu"), "farlink-host");
    }

    #[test]
    fn base64_round_trip() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
        assert_eq!(base64_encode(b""), "");
        for n in 0..300usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 % 256) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data);
        }
        assert!(base64_decode("T").is_none());
        assert!(base64_decode("T!==").is_none());
    }
}
