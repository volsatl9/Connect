//! Connect: peer-to-peer remote desktop.
//!
//! With no arguments this opens the app. The headless commands exist for testing without a display:
//!
//! ```text
//! connect host [--test-pattern[=WxH@FPS]] [--print-input]
//!                                     share this PC (Windows); Allow/Deny is asked on the terminal
//! connect join <USER-ID> [--seconds N]
//! connect id                          print this PC's User-ID
//! connect gpu-check                   check what the graphics hardware offers, and test it
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
//! `CONNECT_RENDERER=glow` if the window stays blank (falls back from wgpu to plain OpenGL).

// No console window behind the app. Debug builds keep theirs, for the logs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod cli;
mod gui;
mod update;

use std::path::PathBuf;

use anyhow::{Result, bail};
use connect_client::Stats;
use connect_net as net;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
usage: connect                        open the app
       connect host [--test-pattern[=WxH@FPS]] [--print-input]
                                      share this PC from the terminal (Windows)
       connect join <USER-ID> [--seconds N]
       connect id                     print this PC's User-ID
       connect gpu-check              check this PC's graphics hardware and save a report";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    if !args.is_empty() {
        attach_parent_console();
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,iroh=warn,noq=warn".into()))
        .init();
    update::clean_up_previous();

    if matches!(args.first().map(String::as_str), Some("-h" | "--help" | "help")) {
        println!("{USAGE}");
        return Ok(());
    }

    let config_dir = match std::env::var_os("CONNECT_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => net::default_config_dir()?,
    };
    let runtime = tokio::runtime::Runtime::new()?;
    // The app asks before creating an account; the terminal commands just make one.
    let key = || net::load_or_create_identity(&config_dir);

    match args.first().map(String::as_str) {
        None => gui::run(runtime, config_dir),
        Some("host") => runtime.block_on(cli::host(key()?, &args[1..])),
        Some("join") => runtime.block_on(cli::join(key()?, &args[1..])),
        Some("gpu-check") => cli::gpu_check(),
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

/// Session figures for the terminal and the in-session overlay: the connection, then where
/// the latency goes.
pub(crate) fn stats_line(s: &Stats) -> String {
    let rtt = s.rtt.map_or("–".into(), |r| format!("{:.0} ms", r.as_secs_f32() * 1000.0));
    let target = s.host_mbps.map_or(String::new(), |m| format!(" (target {m:.1})"));
    let loss = s.loss.map_or(String::new(), |l| format!(" · packet loss {:.1}%", l * 100.0));
    let mut text = format!(
        "{} · RTT {rtt} · {:.0} fps · {:.1} Mb/s{target} · frames lost {} / repaired {} / skipped {}{loss}",
        s.path.label(),
        s.fps,
        s.mbps,
        s.frames_lost,
        s.frames_recovered,
        s.frames_superseded,
    );
    if let Some(l) = &s.latency {
        let ms = |v: f32| format!("{v:.1}");
        let maybe = |v: Option<f32>| v.map_or("–".into(), ms);
        let total = match (l.total, l.worst) {
            (Some(total), Some(worst)) => format!("~{total:.0} ms (slowest {worst:.0})"),
            _ => "measuring…".into(),
        };
        text += &format!(
            "\nLatency to this window {total}: host screen→capture {} · capture {} · encode {} · send wait {} · \
             network {} · reassembly {} · decode {} on {} · draw {} ms",
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
    text
}
