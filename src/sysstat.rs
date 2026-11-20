//! What the helper tells the relay about its computer besides the screens:
//! CPU load, memory and disks ([`SystemStats`], sent with every
//! `AgentMsg::Heartbeat`, together with who is signed in and the
//! workgroup / domain), and the full OS version ([`os_version`]).

use serde::{Deserialize, Serialize};

/// One disk (Linux: a mounted filesystem, Windows: a drive letter).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskStat {
    /// Mount point (`/`, `/home`) or drive (`C:`).
    pub name: String,
    pub total: u64,
    pub free: u64,
}

/// Load and space of a computer at one moment.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemStats {
    /// Average CPU use since the previous sample, 0–100.
    pub cpu: f32,
    pub cores: u32,
    pub mem_total: u64,
    pub mem_used: u64,
    pub disks: Vec<DiskStat>,
    /// Who is signed in on the screen the helper shows ("olga", or
    /// "CORP\\olga" for a domain account); `None` when nobody is (or an
    /// older helper that does not tell).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Windows: the workgroup or Active Directory domain the computer is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined: Option<Joined>,
}

/// Where a Windows computer belongs (JSON: `{"kind":"domain","name":"corp.example.com"}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum Joined {
    Workgroup(String),
    /// Active Directory domain (DNS name when known, else the NetBIOS name).
    Domain(String),
}

impl Joined {
    /// "Workgroup WORKGROUP" / "Domain corp.example.com".
    pub fn label(&self) -> String {
        match self {
            Joined::Workgroup(n) => format!("Workgroup {n}"),
            Joined::Domain(n) => format!("Domain {n}"),
        }
    }
}

/// "olga · Domain corp.example.com", "Nobody signed in · Workgroup WORKGROUP";
/// `None` when the helper told neither.
pub fn user_line(s: &SystemStats) -> Option<String> {
    let joined = s.joined.as_ref().map(Joined::label);
    match (&s.user, joined) {
        (None, None) => None,
        (Some(u), None) => Some(u.clone()),
        (Some(u), Some(j)) => Some(format!("{u} · {j}")),
        (None, Some(j)) => Some(format!("Nobody signed in · {j}")),
    }
}

/// Remembers the previous CPU counters so each sample reports the load
/// since the last one.
#[derive(Default)]
pub struct Sampler {
    prev: Option<(u64, u64)>,
}

impl Sampler {
    /// Take a sample. The very first one measures over a short pause.
    pub fn sample(&mut self) -> SystemStats {
        if self.prev.is_none() {
            self.prev = cpu_times();
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        let now = cpu_times();
        let cpu = busy_between(self.prev, now).unwrap_or(0.0);
        self.prev = now.or(self.prev);
        let (mem_total, mem_used) = memory().unwrap_or((0, 0));
        SystemStats {
            cpu: cpu.clamp(0.0, 100.0),
            cores: std::thread::available_parallelism().map_or(1, |n| n.get() as u32),
            mem_total,
            mem_used,
            disks: disks(),
            user: signed_in_user(),
            joined: joined(),
        }
    }
}

/// CPU use in % between two `(idle, total)` counter readings.
fn busy_between(prev: Option<(u64, u64)>, now: Option<(u64, u64)>) -> Option<f32> {
    match (prev, now) {
        (Some((idle0, total0)), Some((idle1, total1))) if total1 > total0 => {
            let busy = (total1 - total0).saturating_sub(idle1.saturating_sub(idle0));
            Some((busy as f64 * 100.0 / (total1 - total0) as f64).clamp(0.0, 100.0) as f32)
        }
        _ => None,
    }
}

/// Just the CPU part of [`Sampler`], cheap enough to ask every few seconds
/// (the host's screen thread uses it to decide how hard to compress).
#[derive(Default)]
pub struct CpuLoad {
    prev: Option<(u64, u64)>,
}

impl CpuLoad {
    /// CPU use in % since the previous call; `None` the first time.
    pub fn busy(&mut self) -> Option<f32> {
        let now = cpu_times();
        let busy = busy_between(self.prev, now);
        self.prev = now.or(self.prev);
        busy
    }
}

/// Made-up numbers for the demo helper and tests.
pub fn demo_stats() -> SystemStats {
    const G: u64 = 1 << 30;
    SystemStats {
        cpu: 12.5,
        cores: 8,
        mem_total: 16 * G,
        mem_used: 6 * G,
        disks: vec![DiskStat { name: "/".into(), total: 512 * G, free: 201 * G }],
        user: Some("demo".into()),
        joined: None,
    }
}

/// Human size: "512 GB", "7.5 GB", "800 MB".
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 || u == 0 { format!("{v:.0} {}", UNITS[u]) } else { format!("{v:.1} {}", UNITS[u]) }
}

// ------------------------------------------------------------------ Linux

/// (idle, total) jiffies of all CPUs.
#[cfg(not(windows))]
fn cpu_times() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    parse_proc_stat(&s)
}

#[cfg_attr(windows, allow(dead_code))]
fn parse_proc_stat(s: &str) -> Option<(u64, u64)> {
    let line = s.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal (guest is counted in user).
    let total: u64 = v.iter().take(8).sum();
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    Some((idle, total))
}

/// (total, used) bytes.
#[cfg(not(windows))]
fn memory() -> Option<(u64, u64)> {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg_attr(windows, allow(dead_code))]
fn parse_meminfo(s: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':').map(|r| r.split_whitespace().next()?.parse::<u64>().ok()))
            .flatten()
            .map(|kb| kb * 1024)
    };
    let total = field("MemTotal")?;
    let avail = field("MemAvailable").or_else(|| field("MemFree"))?;
    Some((total, total.saturating_sub(avail)))
}

#[cfg(not(windows))]
fn disks() -> Vec<DiskStat> {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else { return Vec::new() };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (dev, dir) in real_mounts(&mounts) {
        if !seen.insert(dev) {
            continue; // same device mounted twice (bind mounts)
        }
        let Ok(c) = std::ffi::CString::new(dir.as_bytes()) else { continue };
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            continue;
        }
        let bs = st.f_frsize as u64;
        let total = st.f_blocks as u64 * bs;
        if total == 0 {
            continue;
        }
        out.push(DiskStat { name: dir, total, free: st.f_bavail as u64 * bs });
    }
    out
}

/// Mounted real disks from `/proc/mounts`: (device, mount point).
#[cfg_attr(windows, allow(dead_code))]
fn real_mounts(mounts: &str) -> Vec<(String, String)> {
    mounts
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (dev, dir, fs) = (it.next()?, it.next()?, it.next()?);
            let dir = dir.replace("\\040", " ");
            let real = dev.starts_with("/dev/") && !dev.starts_with("/dev/loop");
            let skip_fs = matches!(fs, "squashfs" | "iso9660" | "udf");
            let skip_dir = dir.starts_with("/snap/") || dir.starts_with("/boot") || dir.starts_with("/var/snap");
            (real && !skip_fs && !skip_dir).then(|| (dev.to_string(), dir))
        })
        .collect()
}

/// The user running the helper (on Linux it runs in the signed-in user's session).
#[cfg(not(windows))]
fn signed_in_user() -> Option<String> {
    let pw = unsafe { libc::getpwuid(libc::geteuid()) };
    if !pw.is_null() {
        let name = unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) }.to_string_lossy().into_owned();
        if !name.is_empty() {
            return Some(name);
        }
    }
    std::env::var("USER").ok().filter(|u| !u.is_empty())
}

/// Workgroups and AD domains are a Windows thing.
#[cfg(not(windows))]
fn joined() -> Option<Joined> {
    None
}

// ------------------------------------------------------------------ Windows

/// The user of the console session (the screen the helper shows), else of
/// any active remote session; "DOMAIN\\user" unless it is a local account.
#[cfg(windows)]
fn signed_in_user() -> Option<String> {
    use windows_sys::Win32::System::RemoteDesktop::*;
    let query = |session: u32, class: WTS_INFO_CLASS| -> Option<String> {
        let mut buf: windows_sys::core::PWSTR = std::ptr::null_mut();
        let mut len = 0u32;
        if unsafe { WTSQuerySessionInformationW(std::ptr::null_mut(), session, class, &mut buf, &mut len) } == 0 || buf.is_null() {
            return None;
        }
        let text = unsafe { wide_to_string(buf) };
        unsafe { WTSFreeMemory(buf.cast()) };
        Some(text).filter(|t| !t.trim().is_empty())
    };
    let user_of = |session: u32| -> Option<String> {
        let user = query(session, WTSUserName)?;
        let domain = query(session, WTSDomainName);
        Some(account_label(&user, domain.as_deref(), &crate::util::hostname()))
    };
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    if console != u32::MAX
        && let Some(u) = user_of(console)
    {
        return Some(u);
    }
    let mut list: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    if unsafe { WTSEnumerateSessionsW(std::ptr::null_mut(), 0, 1, &mut list, &mut count) } == 0 || list.is_null() {
        return None;
    }
    let sessions = unsafe { std::slice::from_raw_parts(list, count as usize) };
    let found = sessions.iter().filter(|s| s.State == WTSActive).find_map(|s| user_of(s.SessionId));
    unsafe { WTSFreeMemory(list.cast()) };
    found
}

/// Workgroup or domain from `NetGetJoinInformation`; for a domain the DNS
/// name ("corp.example.com") when Windows knows it.
#[cfg(windows)]
fn joined() -> Option<Joined> {
    use windows_sys::Win32::NetworkManagement::NetManagement::*;
    use windows_sys::Win32::System::SystemInformation::{ComputerNameDnsDomain, GetComputerNameExW};
    let mut buf: windows_sys::core::PWSTR = std::ptr::null_mut();
    let mut status: NETSETUP_JOIN_STATUS = 0;
    if unsafe { NetGetJoinInformation(std::ptr::null(), &mut buf, &mut status) } != 0 || buf.is_null() {
        return None;
    }
    let name = unsafe { wide_to_string(buf) };
    unsafe { NetApiBufferFree(buf.cast()) };
    match status {
        s if s == NetSetupWorkgroupName => Some(Joined::Workgroup(name)),
        s if s == NetSetupDomainName => {
            let mut dns = [0u16; 256];
            let mut len = dns.len() as u32;
            let dns = (unsafe { GetComputerNameExW(ComputerNameDnsDomain, dns.as_mut_ptr(), &mut len) } != 0)
                .then(|| String::from_utf16_lossy(&dns[..len as usize]))
                .filter(|d| !d.is_empty());
            Some(Joined::Domain(dns.unwrap_or(name)))
        }
        _ => None,
    }
}

/// A NUL-terminated UTF-16 string.
#[cfg(windows)]
unsafe fn wide_to_string(p: *const u16) -> String {
    let mut n = 0;
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

/// "olga" for a local account (its domain is the computer's own name),
/// "CORP\\olga" for a domain account.
#[cfg_attr(not(windows), allow(dead_code))]
fn account_label(user: &str, domain: Option<&str>, computer: &str) -> String {
    match domain.map(str::trim).filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case(computer)) {
        Some(d) => format!("{d}\\{user}"),
        None => user.to_string(),
    }
}

#[cfg(windows)]
fn cpu_times() -> Option<(u64, u64)> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetSystemTimes;
    let z = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut idle, mut kernel, mut user) = (z(), z(), z());
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let v = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    // Kernel time includes idle time.
    Some((v(idle), v(kernel) + v(user)))
}

#[cfg(windows)]
fn memory() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return None;
    }
    Some((m.ullTotalPhys, m.ullTotalPhys.saturating_sub(m.ullAvailPhys)))
}

#[cfg(windows)]
fn disks() -> Vec<DiskStat> {
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives};
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
        if unsafe { GetDriveTypeW(root.as_ptr()) } != DRIVE_FIXED {
            continue;
        }
        let (mut free, mut total) = (0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) } != 0 && total > 0 {
            out.push(DiskStat { name: format!("{letter}:"), total, free });
        }
    }
    out
}

/// "Windows 11 Pro 23H2 (build 22631)" from the registry.
#[cfg(windows)]
pub fn os_version() -> String {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let read = |name: &str| -> Option<String> {
        let key: Vec<u16> = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion".encode_utf16().chain([0]).collect();
        let val: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let mut buf = [0u16; 256];
        let mut len = (buf.len() * 2) as u32;
        let rc = unsafe {
            RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), val.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len)
        };
        if rc != 0 {
            return None;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..n])).filter(|s| !s.trim().is_empty())
    };
    windows_label(read("ProductName"), read("DisplayVersion").or_else(|| read("ReleaseId")), read("CurrentBuild"))
}

/// Windows 11 still calls itself "Windows 10" in `ProductName`; the build tells.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_label(product: Option<String>, release: Option<String>, build: Option<String>) -> String {
    let mut name = product.unwrap_or_else(|| "Windows".into());
    if build.as_deref().and_then(|b| b.parse::<u32>().ok()).is_some_and(|b| b >= 22000) {
        name = name.replace("Windows 10", "Windows 11");
    }
    if let Some(r) = release {
        name = format!("{name} {r}");
    }
    if let Some(b) = build {
        name = format!("{name} (build {b})");
    }
    name
}

/// "Ubuntu 22.04.4 LTS" (from `/etc/os-release`).
#[cfg(not(windows))]
pub fn os_version() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())))
        .unwrap_or_else(|| "Linux".into())
}

#[cfg(test)]
mod tests {
    use super::busy_between;

    #[test]
    fn busy_share_between_two_readings() {
        // 100 ticks passed, 25 of them idle: 75 % busy.
        assert_eq!(busy_between(Some((1000, 4000)), Some((1025, 4100))), Some(75.0));
        assert_eq!(busy_between(None, Some((1, 2))), None, "first reading");
        assert_eq!(busy_between(Some((5, 10)), Some((5, 10))), None, "no time passed");
        assert_eq!(busy_between(Some((0, 0)), Some((500, 100))), Some(0.0), "odd counters stay in range");
    }

    #[test]
    fn cpu_load_reads_this_machine() {
        let mut l = super::CpuLoad::default();
        assert_eq!(l.busy(), None);
        std::thread::sleep(std::time::Duration::from_millis(50));
        if let Some(b) = l.busy() {
            assert!((0.0..=100.0).contains(&b));
        }
    }

    use super::*;

    #[test]
    fn reads_cpu_memory_and_mounts() {
        let stat = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4\n";
        assert_eq!(parse_proc_stat(stat), Some((850, 1000)));
        let mem = "MemTotal:       16000 kB\nMemFree:  1000 kB\nMemAvailable:    4000 kB\n";
        assert_eq!(parse_meminfo(mem), Some((16000 * 1024, 12000 * 1024)));
        let mounts = "sysfs /sys sysfs rw 0 0\n/dev/sda2 / ext4 rw 0 0\n/dev/loop3 /snap/core squashfs ro 0 0\n\
                      /dev/sda1 /boot/efi vfat rw 0 0\n/dev/sdb1 /media/My\\040Disk ext4 rw 0 0\n";
        assert_eq!(
            real_mounts(mounts),
            vec![("/dev/sda2".into(), "/".into()), ("/dev/sdb1".into(), "/media/My Disk".into())]
        );
    }

    #[test]
    fn names_windows_11_by_its_build() {
        assert_eq!(
            windows_label(Some("Windows 10 Pro".into()), Some("23H2".into()), Some("22631".into())),
            "Windows 11 Pro 23H2 (build 22631)"
        );
        assert_eq!(windows_label(Some("Windows 10 Pro".into()), None, Some("19045".into())), "Windows 10 Pro (build 19045)");
    }

    #[test]
    fn names_users_and_where_the_computer_belongs() {
        assert_eq!(account_label("olga", Some("RENA2"), "rena2"), "olga");
        assert_eq!(account_label("olga", Some("CORP"), "RENA2"), "CORP\\olga");
        assert_eq!(account_label("olga", None, "RENA2"), "olga");
        let mut s = demo_stats();
        s.user = Some("CORP\\olga".into());
        s.joined = Some(Joined::Domain("corp.example.com".into()));
        assert_eq!(user_line(&s).as_deref(), Some("CORP\\olga · Domain corp.example.com"));
        s.user = None;
        s.joined = Some(Joined::Workgroup("WORKGROUP".into()));
        assert_eq!(user_line(&s).as_deref(), Some("Nobody signed in · Workgroup WORKGROUP"));
        s.joined = None;
        assert_eq!(user_line(&s), None, "older helpers tell neither");
        let json = serde_json::to_string(&Joined::Domain("corp".into())).unwrap();
        assert_eq!(json, r#"{"kind":"domain","name":"corp"}"#);
        // Stats from older helpers still read.
        let old: SystemStats = serde_json::from_str(r#"{"cpu":1,"cores":2,"mem_total":3,"mem_used":1,"disks":[]}"#).unwrap();
        assert_eq!((old.user, old.joined), (None, None));
    }

    #[test]
    fn samples_this_computer() {
        let s = Sampler::default().sample();
        assert!(s.mem_total > 0 && s.mem_used <= s.mem_total);
        assert!((0.0..=100.0).contains(&s.cpu));
        assert_eq!(human_bytes(512 << 30), "512 GB");
        assert_eq!(human_bytes(7 << 29), "3.5 GB");
    }
}
