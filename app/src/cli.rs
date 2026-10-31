//! Headless host and join, for testing the real network path without a display.

use std::{
    io::BufRead,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use connect_client::{ClientEvent, Stage};
use connect_host::{Decision, HostConfig, HostEvent, InputSink, VideoSource};
use connect_net::{self as net, iroh::SecretKey};
use connect_proto::Permissions;
use tokio::sync::mpsc;

use crate::{device_name, stats_line};

pub async fn host(key: SecretKey, args: &[String]) -> Result<()> {
    let mut video = VideoSource::default();
    let mut input = InputSink::default();
    for arg in args {
        match arg.as_str() {
            "--test-pattern" => video = VideoSource::TEST_PATTERN,
            // e.g. --test-pattern=1920x1080@144
            spec if spec.starts_with("--test-pattern=") => video = test_pattern(&spec["--test-pattern=".len()..])?,
            "--print-input" => {
                let (tx, mut events) = mpsc::unbounded_channel();
                tokio::spawn(async move {
                    while let Some(event) = events.recv().await {
                        println!("input: {event:?}");
                    }
                });
                input = InputSink::Channel(tx);
            }
            other => bail!("unknown option {other:?} for `connect host`"),
        }
    }
    if video == VideoSource::Screen && !VideoSource::SCREEN_SUPPORTED {
        bail!(
            "hosting needs Windows; this PC can only connect to others (`--test-pattern` serves a test image, for development)"
        );
    }

    let endpoint = net::bind_endpoint(key).await?;
    println!("Your User-ID:  {}", net::format_user_id(&endpoint.id()));
    print!("Going online… ");
    match tokio::time::timeout(Duration::from_secs(15), endpoint.online()).await {
        Ok(()) => println!("online. Give the User-ID to whoever should connect. Ctrl+C stops sharing."),
        Err(_) => println!("no relay reachable yet; only direct connections will work for now."),
    }

    let (host, mut events) =
        connect_host::start(endpoint.clone(), HostConfig { video, input, ..HostConfig::new(device_name()) });
    let mut lines = stdin_lines();

    loop {
        let event = tokio::select! {
            event = events.recv() => event.context("host stopped")?,
            _ = tokio::signal::ctrl_c() => break,
        };
        match event {
            HostEvent::Request(request) => {
                while lines.try_recv().is_ok() {} // ignore anything typed before the question
                println!("\n{} wants to connect to this PC.", request.client_name);
                println!("  Their User-ID: {}", net::format_user_id(&request.peer));
                println!("Allow screen, desktop audio, mouse, keyboard and gamepads? [y/N] (no answer = deny in 30 s)");
                let answer = loop {
                    tokio::select! {
                        line = lines.recv() => break line,
                        _ = tokio::time::sleep(Duration::from_millis(200)) => {
                            if !request.is_pending() {
                                break None;
                            }
                        }
                    }
                };
                match answer {
                    Some(line) if request.is_pending() => {
                        let allow = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
                        println!("{}", if allow { "Allowed." } else { "Denied." });
                        request.respond(if allow { Decision::Allow(Permissions::ALL) } else { Decision::Deny });
                    }
                    _ => println!("The request expired or was withdrawn."),
                }
            }
            HostEvent::SessionStarted { client_name, .. } => println!("{client_name} is connected."),
            HostEvent::SessionEnded { reason, .. } => println!("Session ended: {reason}"),
        }
    }
    host.shutdown().await;
    endpoint.close().await;
    Ok(())
}

fn test_pattern(spec: &str) -> Result<VideoSource> {
    let parse = || -> Option<VideoSource> {
        let (size, fps) = spec.split_once('@')?;
        let (width, height) = size.split_once('x')?;
        Some(VideoSource::TestPattern { width: width.parse().ok()?, height: height.parse().ok()?, fps: fps.parse().ok()? })
    };
    parse().with_context(|| format!("expected WIDTHxHEIGHT@FPS, like 1920x1080@144, not {spec:?}"))
}

pub async fn join(key: SecretKey, args: &[String]) -> Result<()> {
    // A User-ID is often pasted with spaces, so it may arrive split over several arguments.
    let mut id_parts = Vec::new();
    let mut seconds = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => seconds = Some(args.next().context("--seconds needs a number")?.parse::<u64>()?),
            part => id_parts.push(part),
        }
    }
    if id_parts.is_empty() {
        bail!("usage: connect join <USER-ID> [--seconds N]");
    }
    let id = net::parse_user_id(&id_parts.concat())?;

    let endpoint = net::bind_endpoint(key).await?;
    let started = Instant::now();
    // Takes each frame as it's decoded, as a window would, so the stats count what was shown.
    let woken = std::sync::Arc::new(tokio::sync::Notify::new());
    let wake = woken.clone();
    let (client, mut events) = connect_client::connect(endpoint.clone(), id, device_name(), move || wake.notify_one());
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let mut connected_at = None;

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(ClientEvent::Stage(Stage::Connecting)) => println!("Looking up {}…", net::format_user_id(&id)),
                Some(ClientEvent::Stage(Stage::WaitingForApproval)) => println!(
                    "Reached the host in {:.1} s. Waiting for them to allow the connection…",
                    started.elapsed().as_secs_f32()
                ),
                Some(ClientEvent::Accepted { host_name, width, height, permissions }) => {
                    println!("Connected to {host_name}: {width}×{height}, permissions {:#05b}.", permissions.bits());
                    connected_at = Some(Instant::now());
                }
                Some(ClientEvent::PermissionsChanged(p)) => println!("Host changed permissions to {:#05b}.", p.bits()),
                Some(ClientEvent::VideoFormat { width, height }) => println!("Host screen is now {width}×{height}."),
                Some(ClientEvent::InputUnavailable { reason, .. }) => println!("{reason}"),
                Some(ClientEvent::VideoEncoder { name, codec }) => println!("Video: {codec} · {name}"),
                Some(ClientEvent::AudioStatus { active, reason }) => {
                    if let Some(reason) = reason { println!("{reason}"); }
                    else { println!("Desktop audio: {}", if active { "on" } else { "off" }); }
                },
                Some(ClientEvent::Ended(reason)) => {
                    println!("{reason}");
                    break;
                }
                None => break,
            },
            _ = woken.notified() => {
                if let Some(frame) = client.take_frame() {
                    client.recycle(frame);
                }
            }
            _ = ticker.tick(), if connected_at.is_some() => {
                let stats = client.stats();
                if stats.rtt.is_some() {
                    println!("{}", stats_line(&stats));
                }
                if seconds.is_some_and(|s| connected_at.is_some_and(|t| t.elapsed() >= Duration::from_secs(s))) {
                    client.disconnect();
                }
            }
            _ = tokio::signal::ctrl_c() => client.disconnect(),
        }
    }
    endpoint.close().await;
    Ok(())
}

/// Lines typed on the terminal. A plain thread, since stdin reads can't be cancelled.
fn stdin_lines() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Checks what this PC's graphics hardware offers for sharing and viewing, runs the GPU paths
/// briefly, and saves the report for pasting.
pub fn gpu_check() -> Result<()> {
    let mut report = Vec::new();
    let mut out = |line: String| {
        println!("{line}");
        report.push(line);
    };
    out(format!(
        "Connect {} GPU check on {} ({})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        device_name()
    ));
    out("Testing for a few seconds; a video playing on the screen gives more realistic timings.".into());
    connect_client::diagnostics::run(&mut out);
    connect_host::diagnostics::run(&mut out);
    let path = std::env::current_dir().unwrap_or_default().join("connect-gpu-check.txt");
    std::fs::write(&path, report.join("\n") + "\n").with_context(|| format!("saving {}", path.display()))?;
    println!("\nSaved to {}", path.display());
    Ok(())
}
