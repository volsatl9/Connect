//! Connect: peer-to-peer remote desktop.
//!
//! With no arguments this opens the app. The headless commands exist for testing without a display:
//!
//! ```text
//! connect host [--test-pattern[=WxH@FPS]] [--print-input] [--trace DIR]
//!                                     share this PC (Windows); Allow/Deny is asked on the terminal
//! connect join <USER-ID> [--seconds N] [--trace DIR]
//! connect id                          print this PC's User-ID
//! connect gpu-check                   check what the graphics hardware offers, and test it
//! connect trace-report <DIR>           follow every frame through a session's traces
//! ```
//!
//! Only Windows can host; other platforms can only connect. `--test-pattern` hosts a generated
//! image anywhere, and `--print-input` prints the remote input instead of acting on it; both are
//! for development.
//!
//! On Windows the release build is a windowed app, so opening it shows no console. The terminal
//! commands attach to the console they were started from; run them as
//! `start /wait connect.exe …` so the terminal waits for them.
//!
//! Set `CONNECT_CONFIG_DIR` to use a separate identity, e.g. to run two copies on one PC, and
//! `CONNECT_RENDERER=separate` to keep the window off the video decoder's graphics device.
//! `CONNECT_TRACE=DIR` (or `--trace DIR`) writes each session's performance trace there as CSV;
//! `CONNECT_TRACE=0` turns tracing off. See METRICS.md.
//!
//! Hosting, for comparisons: `CONNECT_GPU_CONVERT=video-processor` converts colours with the
//! driver's video processor instead of Connect's shaders; `CONNECT_INTRA_REFRESH=0` goes back to
//! periodic keyframes; `CONNECT_FRAME_BUFFER=<frames>` changes the encoder's buffer setting (0:
//! the driver's default); `CONNECT_PEAK_PERCENT=<n>` uses peak-constrained VBR at n% of the mean;
//! `CONNECT_MIN_QP=<qp>` sets the finest quantizer; `CONNECT_RATE_CEILING=0` runs 0.9.0's rate
//! controller; `CONNECT_PATH_MEMORY=0` runs plain BBR, which forgets the path in quiet scenes and
//! backs off on stray loss;
//! `CONNECT_PROBE_RTT_GAIN=0.5` goes back to BBR's own window while it measures the round trip.
//!
//! Viewing, for telling a driver's glitches from Connect's: `CONNECT_HARDWARE_DECODING=0` decodes
//! on the processor, and `CONNECT_ALLOW_H265=0` asks hosts for H.264.

// No console window behind the app. Debug builds keep theirs, for the logs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod cli;
mod crash;
mod gui;
mod trace_report;
mod update;

use std::{path::PathBuf, sync::Mutex};

use anyhow::{Context, Result, bail};
use connect_client::Stats;
use connect_net as net;
use tracing_subscriber::{filter::Targets, prelude::*};

const USAGE: &str = "\
usage: connect                        open the app
       connect host [--test-pattern[=WxH@FPS]] [--print-input] [--trace DIR]
                                      share this PC from the terminal (Windows)
       connect join <USER-ID> [--seconds N] [--trace DIR]
       connect id                     print this PC's User-ID
       connect gpu-check              check this PC's graphics hardware and save a report
       connect trace-report <DIR>     follow every frame through a session's traces from both PCs";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    if !args.is_empty() {
        attach_parent_console();
    }
    // RUST_LOG as targets and levels only (e.g. "debug,iroh=info"): unlike a full EnvFilter, no
    // regex engine, and spans below the level are never created.
    let filter = std::env::var("RUST_LOG").ok().and_then(|value| value.parse::<Targets>().ok());
    let filter = filter.unwrap_or_else(|| "info,iroh=warn,noq=warn".parse().expect("valid default filter"));
    let config_dir = std::env::var_os("CONNECT_CONFIG_DIR").map(PathBuf::from).or(net::default_config_dir().ok());
    // The window has nowhere to print, so it keeps its log and any crash in the session files.
    let session_files = config_dir.as_deref().filter(|_| args.is_empty()).map(gui::session_files::folder);
    let log = session_files.as_deref().and_then(crash::open_log);
    let log = log.map(|file| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(Mutex::new(file)));
    tracing_subscriber::registry().with(filter).with(tracing_subscriber::fmt::layer()).with(log).init();
    if let Some(folder) = session_files {
        crash::watch(folder);
    }
    update::clean_up_previous();

    if matches!(args.first().map(String::as_str), Some("-h" | "--help" | "help")) {
        println!("{USAGE}");
        return Ok(());
    }

    let config_dir = config_dir.context("no home directory")?;
    // A few workers are plenty for the network and session tasks (video has threads of its own);
    // one per core only costs memory on many-core PCs.
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(4));
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(workers).enable_all().build()?;
    // The app asks before creating an account; the terminal commands just make one.
    let key = || net::load_or_create_identity(&config_dir);

    match args.first().map(String::as_str) {
        None => gui::run(runtime, config_dir),
        Some("host") => runtime.block_on(cli::host(key()?, &args[1..])),
        Some("join") => runtime.block_on(cli::join(key()?, &args[1..])),
        Some("gpu-check") => cli::gpu_check(),
        Some("trace-report") => cli::trace_report(args.get(1).map(String::as_str)),
        Some("id") => {
            println!("{}", net::format_user_id(&key()?.public()));
            Ok(())
        }
        Some(other) => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

/// Lets a terminal command print to the console it was started from; a windowed app has none of
/// its own.
#[cfg(windows)]
fn attach_parent_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // Fails harmlessly when there is no parent console, e.g. output redirected to a file.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// This PC's name as shown to the other side, until the user picks one.
fn device_name() -> String {
    let name = gethostname::gethostname().to_string_lossy().trim().to_owned();
    if name.is_empty() { "My PC".into() } else { name }
}

/// Session figures for the terminal and the in-session overlay: the connection, how smoothly
/// frames come, where the latency goes (percentiles, from the performance trace), input, and
/// what each PC spends on it. METRICS.md says what each figure measures.
pub(crate) fn stats_line(s: &Stats) -> String {
    let rtt = s.rtt.map_or("–".into(), |r| format!("{:.0} ms", r.as_secs_f32() * 1000.0));
    let target = s.host_mbps.map_or(String::new(), |m| {
        let max = s.host_max_mbps.map_or(String::new(), |max| format!(" · maximum {max:.0}"));
        let why = if s.host_reason.is_empty() { String::new() } else { format!(" · {}", s.host_reason) };
        format!(" (target {m:.1}{max}{why})")
    });
    let loss =
        s.loss.map_or(String::new(), |l| format!(" · packet loss {:.1}% · jitter {:.1} ms", l * 100.0, s.jitter_ms));
    let mut text = format!(
        "{} · RTT {rtt} · {:.0} fps · {:.1} Mb/s{target}{loss}\nFrames lost {} · repaired {} · skipped {} · superseded {}",
        s.path.label(),
        s.fps,
        s.mbps,
        s.frames_lost,
        s.frames_recovered,
        s.frames_skipped,
        s.frames_superseded,
    );
    let ms = |us: i64| format!("{:.1}", us as f64 / 1000.0);
    // A stage's time with its unit: µs below a millisecond, where tenths of a millisecond read 0.0.
    let time = |us: i64| if us.abs() < 1000 { format!("{us} µs") } else { format!("{:.1} ms", us as f64 / 1000.0) };
    let p50 = |p: Option<connect_client::Percentiles>| p.map_or("–".into(), |p| time(p.p50));
    if let Some(f) = &s.frames {
        text += &format!(" · last second: dropped {}, repeated ~{}, keyframes {}", s.dropped, f.repeated, f.keyframes);
        if let Some(bytes) = f.frame_bytes {
            // What actually arrived, not the encoder's setting.
            let kb = |b: i64| format!("{:.0}", b as f64 / 1000.0);
            text += &format!(
                " · frames received p50 {} · p95 {} · p99 {} · max {} KB",
                kb(bytes.p50),
                kb(bytes.p95),
                kb(bytes.p99),
                kb(bytes.max)
            );
        }
        if let (Some(time), Some(interval)) = (f.frame_time, f.frame_interval) {
            text += &format!(" · frame time {:.1} ± {:.1} ms (p99 {})", time.mean, time.stddev, ms(interval.p99));
        }
        let clock = s.clock.map_or(String::new(), |c| format!(", clocks ±{} ms", ms(c.uncertainty_us as i64)));
        text += &match f.total {
            Some(t) => format!(
                "\nLatency to this window (est.{clock}): p50 {} · p95 {} · p99 {} · max {} ms",
                ms(t.p50),
                ms(t.p95),
                ms(t.p99),
                ms(t.max)
            ),
            None => "\nLatency to this window: measuring…".into(),
        };
        let host = s.host_perf;
        let from_host =
            |pick: fn(&connect_client::HostPerf) -> Option<connect_client::Percentiles>| host.as_ref().and_then(pick);
        let send = match host {
            Some(h) => format!(
                "held {} · queue {} · packetize {} · send {}",
                p50(h.held),
                p50(h.queue),
                p50(h.packetize),
                p50(h.send)
            ),
            None => format!("send wait {}", p50(f.host_wait)),
        };
        text += &format!(
            "\n  p50 host: screen→capture {} · capture {} · GPU convert {} · encode {} · {send}",
            p50(f.present_to_capture),
            p50(from_host(|h| h.capture).or(f.capture)),
            p50(from_host(|h| h.convert_gpu)),
            p50(from_host(|h| h.encode).or(f.encode)),
        );
        text += &format!(
            "\n  p50 here: network {} (est.) · reassembly {} · decode queue {} · decode {} on {} · draw {}",
            p50(f.network),
            p50(f.reassembly),
            p50(f.decode_queue),
            p50(f.decode),
            if s.gpu_decode { "GPU" } else { "CPU" },
            p50(f.render),
        );
    } else if let Some(l) = &s.latency {
        // Tracing is off: the running averages still work.
        let ms = |v: f32| format!("{v:.1}");
        let maybe = |v: Option<f32>| v.map_or("–".into(), ms);
        let total = match (l.total, l.worst) {
            (Some(total), Some(worst)) => format!("~{total:.0} ms (slowest {worst:.0})"),
            _ => "measuring…".into(),
        };
        text += &format!(
            "\nLatency to this window {total}: host screen→capture {} · capture {} · encode {} · send wait {} · \
             network {} · reassembly {} · decode {} on {} · draw {} ms (averages)",
            maybe(l.present_to_capture),
            ms(l.capture),
            ms(l.encode),
            ms(l.host_wait),
            maybe(l.network),
            ms(l.reassembly),
            ms(l.decode),
            if s.gpu_decode { "GPU" } else { "CPU" },
            ms(l.render),
        );
    }
    let inject = s.host_perf.and_then(|h| h.input_inject);
    if let Some(one_way) = s.input_one_way() {
        text += &format!(
            "\nInput to host (est.): p50 {} · p95 {} · p99 {} ms · injecting {}",
            ms(one_way.p50),
            ms(one_way.p95),
            ms(one_way.p99),
            p50(inject)
        );
    }
    let process = |load: Option<connect_client::ProcessLoad>| {
        load.map(|p| {
            let share = p.cpu_percent / f32::from(p.cores.max(1));
            format!("CPU {share:.1}% ({:.2} cores) · {:.0} MB", p.cpu_percent / 100.0, p.memory_bytes as f64 / 1e6)
        })
    };
    let host_process = s.host_perf.and_then(|h| h.process);
    if host_process.is_some() || s.process.is_some() {
        let unknown = || "–".to_owned();
        text += &format!(
            "\nHost {} · here {}",
            process(host_process).unwrap_or_else(unknown),
            process(s.process).unwrap_or_else(unknown)
        );
    }
    let gpu = |load: Option<connect_client::GpuLoad>| {
        load.map(|g| {
            let mut text = format!("{}% graphics", g.graphics);
            if let Some(encode) = g.encode {
                text += &format!(", {encode}% encoder");
            }
            if let Some(decode) = g.decode {
                text += &format!(", {decode}% decoder");
            }
            text
        })
    };
    if s.host_gpu.is_some() || s.gpu.is_some() {
        let unknown = || "–".to_owned();
        text += &format!(
            "\nGPU load: host {} · here {}",
            gpu(s.host_gpu).unwrap_or_else(unknown),
            gpu(s.gpu).unwrap_or_else(unknown)
        );
    }
    let dropped = s.trace_dropped + s.host_perf.map_or(0, |h| h.trace_dropped);
    if dropped > 0 {
        text += &format!("\n{dropped} trace records dropped (buffer busy)");
    }
    text
}
