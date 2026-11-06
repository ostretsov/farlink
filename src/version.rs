//! Version numbers like `1.4.2` and comparing them for updates.

use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parse `1.2.3`, `v1.2`, `1.2.3-beta` (the suffix is ignored).
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().trim_start_matches(['v', 'V']);
        let core = s.split(['-', '+', ' ']).next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().map(|p| p.parse()).transpose().ok()?.unwrap_or(0);
        let patch = parts.next().map(|p| p.parse()).transpose().ok()?.unwrap_or(0);
        if parts.next().is_some() {
            return None;
        }
        Some(Version { major, minor, patch })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Is `available` newer than `installed`? Unparseable versions never update.
pub fn is_newer(available: &str, installed: &str) -> bool {
    match (Version::parse(available), Version::parse(installed)) {
        (Some(a), Some(b)) => a.cmp(&b) == Ordering::Greater,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(Version::parse("1.2.3"), Some(Version { major: 1, minor: 2, patch: 3 }));
        assert_eq!(Version::parse("v2.0"), Some(Version { major: 2, minor: 0, patch: 0 }));
        assert_eq!(Version::parse("3"), Some(Version { major: 3, minor: 0, patch: 0 }));
        assert_eq!(Version::parse("1.2.3-beta.1"), Some(Version { major: 1, minor: 2, patch: 3 }));
        assert_eq!(Version::parse("abc"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
    }

    #[test]
    fn newer_comparison() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }
}
