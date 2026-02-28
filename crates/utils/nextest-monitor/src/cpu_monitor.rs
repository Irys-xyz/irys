//! Platform-specific CPU monitoring.

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::time::Instant;

    /// Get CPU time (user + system) in ticks for a process (includes all threads)
    fn get_process_cpu_ticks(pid: u32) -> Option<u64> {
        let stat_path = format!("/proc/{}/stat", pid);
        let content = fs::read_to_string(&stat_path).ok()?;

        // Find the end of comm field (it's in parentheses and may contain spaces)
        let comm_end = content.find(')')?;
        let after_comm = &content[comm_end + 2..];
        let fields: Vec<&str> = after_comm.split_whitespace().collect();

        // utime is field index 11, stime is field index 12
        if fields.len() > 12 {
            let utime: u64 = fields[11].parse().ok()?;
            let stime: u64 = fields[12].parse().ok()?;
            Some(utime + stime)
        } else {
            None
        }
    }

    /// Get clock ticks per second
    fn get_clk_tck() -> u64 {
        unsafe { libc::sysconf(libc::_SC_CLK_TCK) as u64 }
    }

    pub struct CpuMonitor {
        pid: u32,
        last_cpu_ticks: u64,
        last_check: Instant,
        clk_tck: u64,
    }

    impl CpuMonitor {
        pub fn new(pid: u32) -> Self {
            let clk_tck = get_clk_tck();
            let initial_ticks = get_process_cpu_ticks(pid).unwrap_or(0);
            Self {
                pid,
                last_cpu_ticks: initial_ticks,
                last_check: Instant::now(),
                clk_tck,
            }
        }

        /// Sample current CPU usage, returns cpu_threads (e.g., 2.5 = 250% CPU)
        pub fn sample(&mut self) -> f64 {
            let now = Instant::now();
            let elapsed = now.duration_since(self.last_check);

            let current_ticks = get_process_cpu_ticks(self.pid).unwrap_or(self.last_cpu_ticks);
            let tick_delta = current_ticks.saturating_sub(self.last_cpu_ticks);

            let elapsed_secs = elapsed.as_secs_f64();
            let cpu_threads = if elapsed_secs > 0.0 {
                (tick_delta as f64 / self.clk_tck as f64) / elapsed_secs
            } else {
                0.0
            };

            self.last_cpu_ticks = current_ticks;
            self.last_check = now;

            cpu_threads
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::process::Command;
    use std::time::Instant;

    pub struct CpuMonitor {
        pid: u32,
        last_check: Instant,
    }

    impl CpuMonitor {
        pub fn new(pid: u32) -> Self {
            Self {
                pid,
                last_check: Instant::now(),
            }
        }

        /// Sample current CPU usage using ps command
        pub fn sample(&mut self) -> f64 {
            self.last_check = Instant::now();

            if let Ok(output) = Command::new("ps")
                .args(["-p", &self.pid.to_string(), "-o", "%cpu="])
                .output()
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Ok(cpu) = stdout.trim().parse::<f64>() {
                    return cpu / 100.0;
                }
            }
            0.0
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    pub struct CpuMonitor {
        _pid: u32,
    }

    impl CpuMonitor {
        pub fn new(pid: u32) -> Self {
            Self { _pid: pid }
        }

        pub fn sample(&mut self) -> f64 {
            0.0
        }
    }
}

pub use platform::CpuMonitor;
