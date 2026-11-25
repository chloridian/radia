#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "windows")]
use std::mem;
#[cfg(target_os = "macos")]
use std::ptr;
use std::thread;

#[cfg(target_os = "macos")]
use libc::{c_void, size_t, sysctlbyname};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::{
    SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
    Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vram {
    pub local_bytes: u64,
    pub budget_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hardware {
    pub cores: usize,
    pub ram_bytes: u64,
    pub vram: Option<Vram>,
}

impl Hardware {
    pub fn detect_cpu_and_ram() -> Self {
        let cores = thread::available_parallelism().map(usize::from).unwrap_or(1).max(1);
        let ram_bytes = total_ram_bytes().unwrap_or(0);
        Self { cores, ram_bytes, vram: None }
    }

    pub fn usable_ram_bytes(&self) -> u64 {
        const MIN_OS_RESERVE: u64 = 2 * 1024 * 1024 * 1024;
        let reserve = MIN_OS_RESERVE.max(self.ram_bytes / 4);
        self.ram_bytes.saturating_sub(reserve)
    }
}

pub fn available_ram_bytes() -> Option<u64> {
    platform_available_ram_bytes()
}

pub fn set_current_thread_below_normal_priority() {
    platform_set_thread_below_normal_priority();
}

#[cfg(target_os = "windows")]
fn platform_set_thread_below_normal_priority() {
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
}

#[cfg(target_os = "linux")]
fn platform_set_thread_below_normal_priority() {
    let _ = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) };
}

#[cfg(target_os = "macos")]
fn platform_set_thread_below_normal_priority() {
    let _ = unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) };
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn platform_set_thread_below_normal_priority() {}

fn total_ram_bytes() -> Option<u64> {
    platform_total_ram_bytes()
}

#[cfg(target_os = "windows")]
fn memory_status() -> Option<MEMORYSTATUSEX> {
    let mut status: MEMORYSTATUSEX = unsafe { mem::zeroed() };
    status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    (ok != 0).then_some(status)
}

#[cfg(target_os = "windows")]
fn platform_total_ram_bytes() -> Option<u64> {
    memory_status().map(|status| status.ullTotalPhys)
}

#[cfg(target_os = "windows")]
fn platform_available_ram_bytes() -> Option<u64> {
    memory_status().map(|status| status.ullAvailPhys)
}

#[cfg(any(target_os = "linux", test))]
fn parse_meminfo(contents: &str) -> (Option<u64>, Option<u64>) {
    let mut total = None;
    let mut available = None;
    for line in contents.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value = rest.split_whitespace().next().and_then(|token| token.parse::<u64>().ok()).and_then(|kib| kib.checked_mul(1024));
        match key.trim() {
            "MemTotal" => total = value,
            "MemAvailable" => available = value,
            _ => {}
        }
    }
    (total, available)
}

#[cfg(target_os = "linux")]
fn platform_total_ram_bytes() -> Option<u64> {
    let contents = fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo(&contents).0
}

#[cfg(target_os = "linux")]
fn platform_available_ram_bytes() -> Option<u64> {
    let contents = fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo(&contents).1
}

#[cfg(target_os = "macos")]
fn platform_total_ram_bytes() -> Option<u64> {
    let mut value: u64 = 0;
    let mut size: size_t = size_of::<u64>();
    let rc = unsafe { sysctlbyname(c"hw.memsize".as_ptr(), (&mut value as *mut u64).cast::<c_void>(), &mut size, ptr::null_mut(), 0) };
    (rc == 0 && size == size_of::<u64>()).then_some(value)
}

#[cfg(target_os = "macos")]
fn platform_available_ram_bytes() -> Option<u64> {
    let total = platform_total_ram_bytes()?;
    let mut level: i32 = 0;
    let mut size: size_t = size_of::<i32>();
    let rc = unsafe {
        sysctlbyname(c"kern.memorystatus_level".as_ptr(), (&mut level as *mut i32).cast::<c_void>(), &mut size, ptr::null_mut(), 0)
    };
    if rc != 0 || size != size_of::<i32>() || !(0..=100).contains(&level) {
        return None;
    }
    let available = (u128::from(total) * u128::from(level as u32)) / 100;
    u64::try_from(available).ok()
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn platform_total_ram_bytes() -> Option<u64> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn platform_available_ram_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_parses() {
        let full = "MemTotal:       16384000 kB\nMemFree:         2048000 kB\nMemAvailable:   12288000 kB\n";
        assert_eq!(parse_meminfo(full), (Some(16_384_000 * 1024), Some(12_288_000 * 1024)));

        let without_available = "MemTotal:       8192000 kB\nMemFree:         1024000 kB\n";
        assert_eq!(parse_meminfo(without_available), (Some(8_192_000 * 1024), None));
    }

    #[test]
    fn detect_reports_nonzero() {
        let hardware = Hardware::detect_cpu_and_ram();
        assert!(hardware.cores >= 1);
        assert!(hardware.ram_bytes > 0);
        assert!(hardware.vram.is_none());
        let available = available_ram_bytes().expect("available RAM reported on host OS");
        assert!(available > 0 && available <= hardware.ram_bytes);
    }
}
