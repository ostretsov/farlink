//! What the helper tells the relay about its computer besides the screens:
//! CPU load, memory and disks ([`SystemStats`], sent with every
//! `AgentMsg::Heartbeat`), and the full OS version ([`os_version`]).

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
        let cpu = match (self.prev, now) {
            (Some((idle0, total0)), Some((idle1, total1))) if total1 > total0 => {
                let busy = (total1 - total0).saturating_sub(idle1.saturating_sub(idle0));
                (busy as f64 * 100.0 / (total1 - total0) as f64) as f32
            }
            _ => 0.0,
        };
        self.prev = now.or(self.prev);
        let (mem_total, mem_used) = memory().unwrap_or((0, 0));
        SystemStats {
            cpu: cpu.clamp(0.0, 100.0),
            cores: std::thread::available_parallelism().map_or(1, |n| n.get() as u32),
            mem_total,
            mem_used,
            disks: disks(),
        }
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

// ------------------------------------------------------------------ Windows

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
    fn samples_this_computer() {
        let s = Sampler::default().sample();
        assert!(s.mem_total > 0 && s.mem_used <= s.mem_total);
        assert!((0.0..=100.0).contains(&s.cpu));
        assert_eq!(human_bytes(512 << 30), "512 GB");
        assert_eq!(human_bytes(7 << 29), "3.5 GB");
    }
}
