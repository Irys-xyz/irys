//! Platform-specific RSS memory monitoring.

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;

    /// Get the page size in bytes (falls back to 4096 if sysconf fails).
    fn get_page_size() -> u64 {
        let raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if raw < 1 { 4096 } else { raw as u64 }
    }

    pub struct MemoryMonitor {
        pid: u32,
        page_size: u64,
    }

    impl MemoryMonitor {
        pub fn new(pid: u32) -> Self {
            Self {
                pid,
                page_size: get_page_size(),
            }
        }

        /// Sample current RSS in bytes.
        /// Reads `/proc/[pid]/statm` field 1 (resident pages) and multiplies by page size.
        pub fn sample(&self) -> u64 {
            let statm_path = format!("/proc/{}/statm", self.pid);
            if let Ok(content) = fs::read_to_string(&statm_path) {
                let fields: Vec<&str> = content.split_whitespace().collect();
                // Field 0 = total pages, Field 1 = resident pages
                if fields.len() > 1 {
                    if let Ok(resident_pages) = fields[1].parse::<u64>() {
                        return resident_pages * self.page_size;
                    }
                }
            }
            0
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::process::Command;

    pub struct MemoryMonitor {
        pid: u32,
    }

    impl MemoryMonitor {
        pub fn new(pid: u32) -> Self {
            Self { pid }
        }

        /// Sample current RSS in bytes using `ps -p <pid> -o rss=` (returns KB).
        pub fn sample(&self) -> u64 {
            if let Ok(output) = Command::new("ps")
                .args(["-p", &self.pid.to_string(), "-o", "rss="])
                .output()
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Ok(rss_kb) = stdout.trim().parse::<u64>() {
                    return rss_kb * 1024;
                }
            }
            0
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    pub struct MemoryMonitor {
        _pid: u32,
    }

    impl MemoryMonitor {
        pub fn new(pid: u32) -> Self {
            Self { _pid: pid }
        }

        pub fn sample(&self) -> u64 {
            0
        }
    }
}

pub use platform::MemoryMonitor;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn test_memory_monitor_self() {
        let pid = std::process::id();
        let monitor = MemoryMonitor::new(pid);
        let rss = monitor.sample();
        // The current process should have some non-zero RSS
        assert!(
            rss > 0,
            "Expected non-zero RSS for current process, got {}",
            rss
        );
        // Should be at least 1MB for any running Rust process
        assert!(rss > 1_000_000, "Expected RSS > 1MB, got {} bytes", rss);
    }
}
