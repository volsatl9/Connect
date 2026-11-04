//! Measurement for Connect's performance tracing: bounded trace rings that never block the
//! threads writing to them, percentiles and spread, this process's CPU and memory use, and CSV
//! export. Nothing here changes how video is captured, sent or shown. See METRICS.md for what
//! each figure means and where it stops being exact.
//!
//! Tracing is on unless `CONNECT_TRACE` is `0` or `off`. Set it to a directory to have each
//! session's trace written there as CSV when the session ends.

use std::{
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

mod csv;
mod process;
mod ring;
mod stats;

pub use csv::{CsvRecord, CsvStream, SessionRow, Spool, cell, write_csv};
pub use process::{ProcessLoad, ProcessMonitor};
pub use ring::TraceRing;
pub use stats::{Percentiles, Spread, percentiles, spread};

/// Records each trace ring keeps: 4096, about 17 s of frames at 240 fps, which the overlay and a
/// quick export need; with a trace directory set, 65 536, about 4.5 minutes. Memory is only
/// touched as records are written.
pub fn ring_capacity() -> usize {
    if trace_dir().is_some() { 65_536 } else { 4_096 }
}

/// Where a session's trace of `what` goes in `dir`: `<role>-<session>-<what>.csv`, with the
/// session's start in Unix seconds, so one session's files sort together.
pub fn trace_path(dir: &Path, role: &str, session: u64, what: &str) -> PathBuf {
    dir.join(format!("{role}-{session}-{what}.csv"))
}

/// Now, in Unix seconds, to name a session's trace files.
pub fn session_stamp() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

static ENABLED: OnceLock<AtomicBool> = OnceLock::new();
static TRACE_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

fn enabled_flag() -> &'static AtomicBool {
    ENABLED.get_or_init(|| {
        let off = std::env::var("CONNECT_TRACE").is_ok_and(|v| matches!(v.trim(), "0" | "off" | "false"));
        AtomicBool::new(!off)
    })
}

/// Whether trace records are kept. One relaxed atomic load: cheap enough for every frame.
#[inline]
pub fn enabled() -> bool {
    enabled_flag().load(Ordering::Relaxed)
}

/// Turns tracing on or off for the whole process (e.g. from a command-line flag).
pub fn set_enabled(on: bool) {
    enabled_flag().store(on, Ordering::Relaxed);
}

/// Where sessions write their trace at the end, if anywhere: `CONNECT_TRACE` when it names a
/// directory, or what [`set_trace_dir`] chose first.
pub fn trace_dir() -> Option<PathBuf> {
    TRACE_DIR
        .get_or_init(|| {
            std::env::var_os("CONNECT_TRACE")
                .filter(|v| {
                    !matches!(v.to_str().map(str::trim), Some("" | "0" | "1" | "on" | "off" | "true" | "false"))
                })
                .map(PathBuf::from)
        })
        .clone()
}

/// Sets the directory for end-of-session traces, unless one is already set. Returns whether
/// this call set it.
pub fn set_trace_dir(dir: PathBuf) -> bool {
    TRACE_DIR.set(Some(dir)).is_ok()
}

/// The high-resolution performance counter (Windows QPC), for joining traces with tools that
/// log it, such as PresentMon (`--qpc_time`). 0 where there is none.
#[inline]
pub fn qpc_now() -> u64 {
    #[cfg(windows)]
    {
        let mut counter = 0i64;
        // SAFETY: writes the counter to a valid i64.
        let _ = unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut counter) };
        counter.max(0) as u64
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Ticks per second of [`qpc_now`]; 0 where there is no such counter.
pub fn qpc_frequency() -> u64 {
    #[cfg(windows)]
    {
        let mut frequency = 0i64;
        // SAFETY: writes the frequency to a valid i64.
        let _ = unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency) };
        frequency.max(0) as u64
    }
    #[cfg(not(windows))]
    {
        0
    }
}
