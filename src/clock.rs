//! `mach_absolute_time` for short latency samples.
//!
//! Throughput runs use `Instant` over hundreds of milliseconds, which is
//! accurate. A single allocation is only tens of nanoseconds, so latency
//! samples read the mach tick counter directly and subtract the cost of the
//! read itself.

#[repr(C)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebase) -> i32;
}

#[derive(Clone, Copy)]
pub struct Clock {
    numer: u64,
    denom: u64,
    overhead_ticks: u64,
}

impl Clock {
    pub fn new() -> Self {
        let mut info = MachTimebase { numer: 0, denom: 1 };
        let rc = unsafe {
            // SAFETY: `info` is a valid out-pointer for mach_timebase_info.
            mach_timebase_info(&mut info)
        };
        assert_eq!(rc, 0, "mach_timebase_info failed");

        let mut overhead = u64::MAX;
        for _ in 0..4_000 {
            let start = ticks();
            std::hint::black_box(start);
            let end = ticks();
            overhead = overhead.min(end.saturating_sub(start));
        }

        Self {
            numer: u64::from(info.numer),
            denom: u64::from(info.denom).max(1),
            overhead_ticks: overhead,
        }
    }

    pub fn resolution_ns(&self) -> u64 {
        self.numer / self.denom.max(1)
    }

    /// Nanoseconds between two ticks, with the empty-read overhead removed.
    pub fn elapsed_ns(&self, start: u64, end: u64) -> u64 {
        let ticks = end
            .saturating_sub(start)
            .saturating_sub(self.overhead_ticks);
        ticks.saturating_mul(self.numer) / self.denom
    }
}

#[inline]
pub fn ticks() -> u64 {
    unsafe {
        // SAFETY: mach_absolute_time has no arguments and does not unwind.
        mach_absolute_time()
    }
}

pub fn read_sysctl(name: &str) -> Option<String> {
    let output = std::process::Command::new("sysctl")
        .args(["-n", name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}
