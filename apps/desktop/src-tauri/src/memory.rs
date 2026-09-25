//! Picks how much memory Gather's local stack may use. Machines under
//! [`LOW_MEMORY_BELOW`] of RAM (4 GB laptops) get the low profile: smaller
//! PostgreSQL buffers here, and lower daemon defaults via
//! `GATHER_MEMORY_PROFILE=low`. `GATHER_MEMORY_PROFILE=low|standard` in the
//! app's environment overrides the detection.

use serde::Serialize;

/// RAM below which the low profile is chosen automatically. A "4 GB" machine
/// reports a little less than 4 GiB; 8 GB machines stay on standard.
const LOW_MEMORY_BELOW: u64 = 6 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Standard,
    Low,
}

impl Profile {
    /// The value the daemon reads from GATHER_MEMORY_PROFILE.
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Standard => "standard",
            Profile::Low => "low",
        }
    }

    /// Server settings passed to `postgres` at every start, so they follow
    /// the profile even for a database created under the other one.
    pub fn postgres_settings(self) -> &'static [&'static str] {
        match self {
            Profile::Standard => &[],
            Profile::Low => &[
                "shared_buffers=64MB",
                "work_mem=2MB",
                "maintenance_work_mem=32MB",
                "effective_cache_size=256MB",
                // The daemon's pool is 4 connections in this profile.
                "max_connections=12",
                "autovacuum_max_workers=1",
                "max_parallel_workers_per_gather=0",
                "jit=off",
            ],
        }
    }
}

/// The daemon's per-file limit in MiB, worked out the way the daemon does:
/// GATHER_MAX_UPLOAD_MB when set (the daemon inherits this app's
/// environment), otherwise the profile's default.
pub fn max_upload_mb(profile: Profile) -> u64 {
    upload_cap_mb(
        std::env::var("GATHER_MAX_UPLOAD_MB").ok().as_deref(),
        profile,
    )
}

fn upload_cap_mb(setting: Option<&str>, profile: Profile) -> u64 {
    setting
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(match profile {
            Profile::Standard => 256,
            Profile::Low => 32,
        })
}

/// The profile in use and why, for the Settings page.
#[derive(Clone, Debug, Serialize)]
pub struct MemoryInfo {
    pub profile: Profile,
    /// Total RAM in MiB, when it could be read.
    pub total_mb: Option<u64>,
    /// True when GATHER_MEMORY_PROFILE chose the profile, not detection.
    pub overridden: bool,
}

pub fn current() -> MemoryInfo {
    let total = total_ram_bytes();
    let setting = std::env::var("GATHER_MEMORY_PROFILE").ok();
    let (profile, overridden) = choose(setting.as_deref(), total);
    MemoryInfo {
        profile,
        total_mb: total.map(|b| b / (1024 * 1024)),
        overridden,
    }
}

/// An explicit `low`/`standard` wins; anything else (unset, `auto`) means
/// detect. Unknown RAM keeps the standard profile.
fn choose(setting: Option<&str>, total_bytes: Option<u64>) -> (Profile, bool) {
    match setting.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("low") => (Profile::Low, true),
        Some("standard") => (Profile::Standard, true),
        _ => match total_bytes {
            Some(total) if total < LOW_MEMORY_BELOW => (Profile::Low, false),
            _ => (Profile::Standard, false),
        },
    }
}

#[cfg(target_os = "linux")]
fn total_ram_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(kib * 1024)
}

#[cfg(target_os = "macos")]
fn total_ram_bytes() -> Option<u64> {
    let out = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(windows)]
fn total_ram_bytes() -> Option<u64> {
    // MEMORYSTATUSEX, from <sysinfoapi.h>.
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    // SAFETY: `status` is a correctly sized MEMORYSTATUSEX with `length` set,
    // as the API requires; it is only written during the call.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    (ok != 0).then_some(status.total_phys)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn total_ram_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn small_machines_get_the_low_profile() {
        // A 4 GB laptop reports a little under 4 GiB.
        assert_eq!(choose(None, Some(3_900_000_000)), (Profile::Low, false));
        assert_eq!(choose(Some("auto"), Some(5 * GIB)), (Profile::Low, false));
        assert_eq!(choose(None, Some(8 * GIB)), (Profile::Standard, false));
        assert_eq!(choose(None, None), (Profile::Standard, false));
    }

    #[test]
    fn an_explicit_setting_wins() {
        assert_eq!(choose(Some("low"), Some(64 * GIB)), (Profile::Low, true));
        assert_eq!(
            choose(Some(" Standard "), Some(2 * GIB)),
            (Profile::Standard, true)
        );
    }

    #[test]
    fn upload_cap_matches_the_daemon() {
        assert_eq!(upload_cap_mb(None, Profile::Low), 32);
        assert_eq!(upload_cap_mb(Some(""), Profile::Standard), 256);
        assert_eq!(upload_cap_mb(Some("64"), Profile::Low), 64);
        assert_eq!(upload_cap_mb(Some("lots"), Profile::Low), 32);
    }

    #[test]
    fn this_machine_reports_its_ram() {
        assert!(total_ram_bytes().is_some_and(|b| b > 256 * 1024 * 1024));
    }
}
