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
    /// Network traffic since the previous sample (helpers from 0.1.18 on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<NetRate>,
}

/// Average network traffic over a while, in bytes per second, of the
/// computer's real network cards (no loopback, bridges or virtual links).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct NetRate {
    /// Received.
    #[serde(rename = "in")]
    pub rx: f64,
    /// Sent.
    #[serde(rename = "out")]
    pub tx: f64,
}

/// Total bytes `(received, sent)` read at `at`.
type NetCounters = (u64, u64);

/// Bytes per second between two counter readings `secs` apart; `None`
/// without both readings or time between them. A counter that went down
/// (a network card disappeared) counts as nothing.
fn rate_between(prev: Option<NetCounters>, now: Option<NetCounters>, secs: f64) -> Option<NetRate> {
    let ((rx0, tx0), (rx1, tx1)) = (prev?, now?);
    (secs > 0.0).then(|| NetRate { rx: rx1.saturating_sub(rx0) as f64 / secs, tx: tx1.saturating_sub(tx0) as f64 / secs })
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
    prev_net: Option<(std::time::Instant, NetCounters)>,
}

impl Sampler {
    /// Take a sample. The very first one measures over a short pause.
    pub fn sample(&mut self) -> SystemStats {
        if self.prev.is_none() {
            self.prev = cpu_times();
            self.prev_net = net_counters().map(|c| (std::time::Instant::now(), c));
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        let now = cpu_times();
        let cpu = busy_between(self.prev, now).unwrap_or(0.0);
        self.prev = now.or(self.prev);
        let net_now = net_counters().map(|c| (std::time::Instant::now(), c));
        let net = match (self.prev_net, net_now) {
            (Some((t0, c0)), Some((t1, c1))) => rate_between(Some(c0), Some(c1), t1.duration_since(t0).as_secs_f64()),
            _ => None,
        };
        self.prev_net = net_now.or(self.prev_net);
        let (mem_total, mem_used) = memory().unwrap_or((0, 0));
        SystemStats {
            cpu: cpu.clamp(0.0, 100.0),
            cores: std::thread::available_parallelism().map_or(1, |n| n.get() as u32),
            mem_total,
            mem_used,
            disks: disks(),
            user: signed_in_user(),
            joined: joined(),
            net,
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

/// How far back the relay remembers CPU and memory use.
pub const LOAD_WINDOW: u64 = 600;

/// CPU, memory and network use at one heartbeat
/// (JSON: `{"t":…,"cpu":…,"mem":…,"net":{"in":…,"out":…}}`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LoadPoint {
    /// Unix seconds (relay clock).
    pub t: u64,
    /// CPU busy, 0–100.
    pub cpu: f32,
    /// Memory used, 0–100.
    pub mem: f32,
    /// Network traffic (`None` from helpers before 0.1.18).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<NetRate>,
}

/// The last [`LOAD_WINDOW`] seconds of heartbeats of one computer (relay memory only).
#[derive(Debug, Clone, Default)]
pub struct LoadHistory(std::collections::VecDeque<LoadPoint>);

impl LoadHistory {
    pub fn push(&mut self, now: u64, s: &SystemStats) {
        let mem = (s.mem_used as f64 * 100.0 / s.mem_total.max(1) as f64) as f32;
        self.0.push_back(LoadPoint { t: now, cpu: s.cpu.clamp(0.0, 100.0), mem: mem.clamp(0.0, 100.0), net: s.net });
        self.prune(now);
    }

    fn prune(&mut self, now: u64) {
        while self.0.front().is_some_and(|p| p.t + LOAD_WINDOW < now) {
            self.0.pop_front();
        }
    }

    /// Points of the last [`LOAD_WINDOW`] seconds, oldest first.
    pub fn recent(&self, now: u64) -> Vec<LoadPoint> {
        self.0.iter().filter(|p| p.t + LOAD_WINDOW >= now).copied().collect()
    }
}

/// Made-up history for the demo: a heartbeat every 30 s for 10 minutes.
pub fn demo_load(now: u64, cpu: f32, mem: f32, seed: u32) -> Vec<LoadPoint> {
    (0..=20u32)
        .map(|i| {
            let w = ((i * 7 + seed * 13) % 11) as f32 / 10.0 - 0.5;
            let spike = if (i + seed) % 9 == 4 { 35.0 } else { 0.0 };
            // A download a few minutes ago, otherwise a little background traffic.
            let busy = (8..12).contains(&((i + seed * 3) % 20));
            let base = 40_000.0 + 25_000.0 * f64::from(w);
            LoadPoint {
                t: now - u64::from(20 - i) * 30,
                cpu: if i == 20 { cpu } else { (cpu + w * 18.0 + spike).clamp(1.0, 100.0) },
                mem: if i == 20 { mem } else { (mem + w * 4.0 - (20 - i) as f32 * 0.2).clamp(1.0, 100.0) },
                net: Some(NetRate {
                    rx: if busy { 2_400_000.0 + 600_000.0 * f64::from(w) } else { base * 3.0 },
                    tx: if busy { 90_000.0 } else { base },
                }),
            }
        })
        .collect()
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
        net: Some(NetRate { rx: 120_000.0, tx: 40_000.0 }),
    }
}

/// "0 B/s", "850 B/s", "120 KB/s", "2.4 MB/s".
pub fn human_rate(bytes_per_sec: f64) -> String {
    format!("{}/s", human_bytes(bytes_per_sec.max(0.0).round() as u64))
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

/// Bytes received and sent by the real network cards since boot: every
/// interface in `/proc/net/dev` that is not virtual (loopback, docker and
/// libvirt bridges, veth, VPN tunnels: their traffic also crosses a real card).
#[cfg(not(windows))]
fn net_counters() -> Option<NetCounters> {
    let s = std::fs::read_to_string("/proc/net/dev").ok()?;
    parse_net_dev(&s, |name| std::path::Path::new("/sys/devices/virtual/net").join(name).exists())
}

/// Sum of `/proc/net/dev` over the interfaces `virtual_if` does not reject.
#[cfg_attr(windows, allow(dead_code))]
fn parse_net_dev(s: &str, virtual_if: impl Fn(&str) -> bool) -> Option<NetCounters> {
    let mut total: Option<NetCounters> = None;
    for line in s.lines() {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();
        if name.is_empty() || name == "lo" || virtual_if(name) {
            continue;
        }
        // Receive: bytes packets errs drop fifo frame compressed multicast; then transmit: bytes …
        let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if v.len() < 9 {
            continue;
        }
        let t = total.get_or_insert((0, 0));
        t.0 += v[0];
        t.1 += v[8];
    }
    total
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

/// Bytes received and sent by the hardware network cards (not loopback,
/// not the filter layers Windows lists for each card, which would count twice).
#[cfg(windows)]
fn net_counters() -> Option<NetCounters> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, IF_TYPE_SOFTWARE_LOOPBACK, MIB_IF_TABLE2};
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    if unsafe { GetIfTable2(&mut table) } != 0 || table.is_null() {
        return None;
    }
    let mut total: Option<NetCounters> = None;
    unsafe {
        let n = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), n);
        for r in rows {
            let flags = r.InterfaceAndOperStatusFlags._bitfield;
            let hardware = flags & 1 != 0;
            let filter = flags & 2 != 0;
            if !hardware || filter || r.Type == IF_TYPE_SOFTWARE_LOOPBACK || r.OperStatus != IfOperStatusUp {
                continue;
            }
            let t = total.get_or_insert((0, 0));
            t.0 += r.InOctets;
            t.1 += r.OutOctets;
        }
        FreeMibTable(table as *const _);
    }
    total
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
    fn reads_network_traffic_of_real_cards() {
        let dev = "Inter-|   Receive                                                |  Transmit\n \
             face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
             lo: 5000 50 0 0 0 0 0 0 5000 50 0 0 0 0 0 0\n  \
             eth0: 1000 10 0 0 0 0 0 0 300 3 0 0 0 0 0 0\n\
             wlan0:200 2 0 0 0 0 0 0 100 1 0 0 0 0 0 0\n\
             docker0: 7777 7 0 0 0 0 0 0 7777 7 0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(dev, |n| n == "docker0"), Some((1200, 400)), "no loopback, no virtual bridge; `wlan0:200` has no space");
        assert_eq!(parse_net_dev(dev, |_| true), None, "only virtual interfaces: nothing to tell");
        assert_eq!(parse_net_dev("garbage\nno: numbers here\n", |_| false), None);

        assert_eq!(rate_between(Some((1000, 500)), Some((4000, 1100)), 30.0), Some(NetRate { rx: 100.0, tx: 20.0 }));
        assert_eq!(rate_between(Some((9000, 900)), Some((100, 1000)), 10.0), Some(NetRate { rx: 0.0, tx: 10.0 }), "a card went away");
        assert_eq!(rate_between(None, Some((1, 1)), 30.0), None, "first reading");
        assert_eq!(rate_between(Some((1, 1)), Some((2, 2)), 0.0), None, "no time passed");

        assert_eq!(human_rate(0.0), "0 B/s");
        assert_eq!(human_rate(850.4), "850 B/s");
        assert_eq!(human_rate(2.5 * 1024.0 * 1024.0), "2.5 MB/s");
        assert_eq!(human_rate(-3.0), "0 B/s");

        // JSON names and older helpers.
        let s = serde_json::to_string(&NetRate { rx: 1.5, tx: 2.0 }).unwrap();
        assert_eq!(s, r#"{"in":1.5,"out":2.0}"#);
        let old: LoadPoint = serde_json::from_str(r#"{"t":5,"cpu":1,"mem":2}"#).unwrap();
        assert_eq!(old.net, None);
    }

    #[test]
    fn samples_this_computer() {
        let mut sampler = Sampler::default();
        let s = sampler.sample();
        assert!(s.mem_total > 0 && s.mem_used <= s.mem_total);
        assert!((0.0..=100.0).contains(&s.cpu));
        // Network: measured over the first pause already (if this computer has a real card).
        if net_counters().is_some() {
            let n = s.net.expect("network rate from the first sample");
            assert!(n.rx >= 0.0 && n.tx >= 0.0);
            assert!(sampler.sample().net.is_some());
        }
        assert_eq!(human_bytes(512 << 30), "512 GB");
        assert_eq!(human_bytes(7 << 29), "3.5 GB");
    }

    #[test]
    fn load_history_keeps_ten_minutes() {
        let mut h = super::LoadHistory::default();
        let mut s = super::demo_stats();
        for i in 0..30u64 {
            s.cpu = i as f32;
            h.push(1000 + i * 30, &s);
        }
        let now = 1000 + 29 * 30;
        let pts = h.recent(now);
        assert_eq!(pts.len(), 21);
        assert_eq!(pts.first().unwrap().t, now - super::LOAD_WINDOW);
        assert_eq!(pts.last().unwrap().cpu, 29.0);
        assert!((pts[0].mem - 37.5).abs() < 0.01);
        assert_eq!(pts[0].net, s.net, "network traffic is kept too");
        // A computer that went quiet: nothing old is shown.
        assert!(h.recent(now + 601).is_empty());
    }
}
