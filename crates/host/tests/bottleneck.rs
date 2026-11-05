//! Video through a path slower than the host's bitrate cap: a relay between host and client that
//! forwards at a fixed rate through a small queue, dropping packets that find it full, as a home
//! router or an ISP's link does. The source is noise the encoder can't compress, so the video
//! uses all the bitrate it's given, as a busy game does. What matters is whether the stream
//! settles under the path's rate without losing frames, and what its latency does meanwhile.
//!
//! `cargo test -p connect-host --release --test bottleneck -- --ignored --nocapture`
//! (`BOTTLENECK_MBPS`, `BOTTLENECK_QUEUE_KB`, `BOTTLENECK_SECS`, `BOTTLENECK_FPS` change the path
//! and the run; `CONNECT_RATE_CEILING=0` runs 0.9.0's rate controller, to compare.)
//!
//! `BOTTLENECK_DROP_SECS` makes the path four times slower that far in, `BOTTLENECK_BACK_SECS`
//! brings it back, and `BOTTLENECK_CAP_MBPS` sets the host's cap.
//!
//! `replaying_a_session` sends a real session's frames, sizes and timing from a host trace
//! (`REPLAY_FRAMES=host-…-frames.csv`, `REPLAY_FROM` and `REPLAY_TO` in seconds), over the same
//! kind of path with room to spare (`REPLAY_MBPS`), optionally scaled (`REPLAY_SCALE`). Synthetic traffic doesn't make BBR forget the path; a game
//! does. `CONNECT_PATH_MEMORY=0` shows plain BBR. `REPLAY_DELAY_MS` sets the delay each way,
//! `REPLAY_IDLE_MS` the wait between connecting and the first frame.
//!
//! `REPLAY_INPUT_HZ` also sends input the other way at that rate, as a mouse does, and reports how
//! long it took over the path's delay.
//!
//! Both can wander the path's delay (`PATH_JITTER_MS`) and lose packets whatever is being sent
//! (`PATH_LOSS_PERCENT`, from `PATH_LOSS_FROM` to `PATH_LOSS_TO` seconds in).

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use connect_client::{ClientEvent, Stats};
use connect_host::{Decision, HostConfig, HostEvent, VideoSource};
use connect_net::{self as net, iroh::EndpointAddr};
use connect_proto::Permissions;
use iroh::SecretKey;
use tokio::{net::UdpSocket, sync::mpsc, time::timeout};

/// The path's shape.
#[derive(Clone, Copy)]
struct Path {
    /// Host → client rate.
    bits_per_second: f64,
    /// The bottleneck's queue: packets arriving while it holds this much are dropped.
    queue_bytes: usize,
    /// Each way, on top of the queue.
    delay: Duration,
    /// When the path gets four times slower, if it does, and when it gets better again.
    slows_at: Option<Duration>,
    back_at: Option<Duration>,
    /// How far the one-way delay wanders over `delay`: on the internet the lowest round trip
    /// seen is well under the usual one (37 and 44 ms at Kellu).
    jitter: Duration,
    /// When packets go missing on the way, and the share of them, whatever is being sent.
    stray_loss: Option<(Duration, Duration, f64)>,
}

/// The stray loss a test's environment asks for, in seconds and percent.
fn stray_loss_from_env() -> Option<(Duration, Duration, f64)> {
    let share = env("PATH_LOSS_PERCENT", 0.0f64) / 100.0;
    (share > 0.0).then(|| {
        (Duration::from_secs(env("PATH_LOSS_FROM", 0)), Duration::from_secs(env("PATH_LOSS_TO", u64::MAX / 2)), share)
    })
}

/// Enough randomness for a test path.
fn next_random(state: &mut u64) -> f64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state as f64 / u64::MAX as f64
}

/// What got through the relay.
#[derive(Default)]
struct Relayed {
    forwarded: u64,
    dropped: u64,
}

/// Forwards UDP between the client (IPv4) and `host` (IPv6) along `path`. Neither can reach the
/// other's own address, so nothing goes round the relay.
async fn bottleneck_relay(host: SocketAddr, path: Path, relayed: Arc<Mutex<Relayed>>) -> SocketAddr {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let host_side = Arc::new(UdpSocket::bind("[::1]:0").await.unwrap());
    let addr = socket.local_addr().unwrap();
    // Packets go out in order, each at its time, from one task per direction.
    let sender = |socket: Arc<UdpSocket>| {
        let (tx, mut rx) = mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>, SocketAddr)>();
        tokio::spawn(async move {
            while let Some((at, packet, to)) = rx.recv().await {
                tokio::time::sleep_until(at).await;
                let _ = socket.send_to(&packet, to).await;
            }
        });
        tx
    };
    let (to_client, to_host) = (sender(socket.clone()), sender(host_side.clone()));
    let client: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    // Client → host: only the delay.
    let from_client = client.clone();
    tokio::spawn(async move {
        let mut buf = vec![0; 65_536];
        loop {
            let Ok((len, from)) = socket.recv_from(&mut buf).await else { return };
            *from_client.lock().unwrap() = Some(from);
            let _ = to_host.send((tokio::time::Instant::now() + path.delay, buf[..len].to_vec(), host));
        }
    });
    // Host → client: through the bottleneck.
    tokio::spawn(async move {
        let mut buf = vec![0; 65_536];
        // When the bottleneck finishes sending what it holds.
        let mut free_at = tokio::time::Instant::now();
        let started = Instant::now();
        let mut random = 0x9E37_79B9_7F4A_7C15u64;
        // The delay moves in steps, not per packet: packets stay in order, so the slowest of a
        // run would otherwise set the delay for all of them.
        let (mut wander, mut wander_until) = (Duration::ZERO, started);
        loop {
            let Ok((len, _)) = host_side.recv_from(&mut buf).await else { return };
            let slowed = path.slows_at.is_some_and(|at| started.elapsed() >= at)
                && path.back_at.is_none_or(|at| started.elapsed() < at);
            let bits_per_second = if slowed { path.bits_per_second / 4.0 } else { path.bits_per_second };
            let Some(client) = *client.lock().unwrap() else { continue };
            if path.stray_loss.is_some_and(|(from, to, share)| {
                (from..to).contains(&started.elapsed()) && next_random(&mut random) < share
            }) {
                relayed.lock().unwrap().dropped += 1;
                continue;
            }
            if Instant::now() >= wander_until {
                wander = path.jitter.mul_f64(next_random(&mut random));
                wander_until = Instant::now() + Duration::from_millis(50);
            }
            let now = tokio::time::Instant::now();
            free_at = free_at.max(now);
            let held = (free_at - now).as_secs_f64() * bits_per_second / 8.0;
            if held as usize + len > path.queue_bytes {
                relayed.lock().unwrap().dropped += 1;
                continue;
            }
            free_at += Duration::from_secs_f64(len as f64 * 8.0 / bits_per_second);
            relayed.lock().unwrap().forwarded += 1;
            let _ = to_client.send((free_at + path.delay + wander, buf[..len].to_vec(), client));
        }
    });
    addr
}

fn seconds_from(name: &str) -> Option<Duration> {
    std::env::var(name).ok().map(|s| Duration::from_secs(s.parse().unwrap()))
}

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// One stats window as the overlay shows it, the latency from the trace's percentiles.
fn line(second: u64, s: &Stats) -> String {
    let ms = |us: i64| us as f64 / 1000.0;
    let latency = s
        .frames
        .and_then(|f| f.total)
        .map_or("–".into(), |t| format!("p50 {:5.1} p99 {:5.1} max {:5.1}", ms(t.p50), ms(t.p99), ms(t.max)));
    let keyframes = s.frames.map_or(0, |f| f.keyframes);
    format!(
        "{second:3}s  {:5.1} Mb/s (target {:5.1})  {:4.0} fps  lost {:3} skipped {:3}  keyframes {keyframes}  latency {latency}",
        s.mbps,
        s.host_mbps.unwrap_or(0.0),
        s.fps,
        s.frames_lost,
        s.frames_skipped
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "half a minute of video through a slow path"]
async fn video_through_a_bottleneck() {
    let path = Path {
        bits_per_second: env("BOTTLENECK_MBPS", 10.0) * 1e6,
        queue_bytes: env("BOTTLENECK_QUEUE_KB", 64) * 1024,
        delay: Duration::from_millis(20),
        slows_at: seconds_from("BOTTLENECK_DROP_SECS"),
        back_at: seconds_from("BOTTLENECK_BACK_SECS"),
        jitter: Duration::from_millis(env("PATH_JITTER_MS", 0)),
        stray_loss: stray_loss_from_env(),
    };
    let seconds = env("BOTTLENECK_SECS", 30u64);
    let fps = env("BOTTLENECK_FPS", 120u32);

    let cap = env("BOTTLENECK_CAP_MBPS", 20);

    let host_ep = net::bind_local_endpoint_at(SecretKey::generate(), "[::1]:0").await.unwrap();
    let config = HostConfig {
        video: VideoSource::NoisyTestPattern { width: 1280, height: 720, fps },
        bitrate: connect_host::BitrateSettings { automatic: false, max_mbps: cap, min_mbps: 2 },
        ..HostConfig::new("Bottleneck host")
    };
    let (host, mut host_events) = connect_host::start(host_ep.clone(), config);
    let host_addr = *host_ep.bound_sockets().first().unwrap();
    let relayed = Arc::new(Mutex::new(Relayed::default()));
    let relay = bottleneck_relay(host_addr, path, relayed.clone()).await;

    let client_ep = net::bind_local_endpoint(SecretKey::generate()).await.unwrap();
    let addr = EndpointAddr::new(host_ep.id()).with_ip_addr(relay);
    let (client, mut events) = connect_client::connect(client_ep.clone(), addr, "Bottleneck client", || {});
    match timeout(Duration::from_secs(20), host_events.recv()).await.unwrap().unwrap() {
        HostEvent::Request(request) => request.respond(Decision::Allow(Permissions::VIEW_ONLY)),
        other => panic!("expected a request, got {other:?}"),
    }
    loop {
        match timeout(Duration::from_secs(20), events.recv()).await.unwrap().unwrap() {
            ClientEvent::Accepted { .. } => break,
            ClientEvent::Ended(reason) => panic!("ended: {reason}"),
            _ => {}
        }
    }

    println!(
        "path {:.0} Mb/s, queue {} KB, {} ms each way; host cap {cap} Mb/s",
        path.bits_per_second / 1e6,
        path.queue_bytes / 1024,
        path.delay.as_millis()
    );
    let started = Instant::now();
    let mut next_report = 1;
    let mut worst: Vec<i64> = Vec::new();
    while started.elapsed() < Duration::from_secs(seconds) {
        if let Some(mut frame) = client.take_frame() {
            frame.timing.mark_painted();
            client.recycle(frame);
        }
        if started.elapsed() >= Duration::from_secs(next_report) {
            let stats = client.stats();
            println!("{}", line(next_report, &stats));
            if let Some(total) = stats.frames.and_then(|f| f.total) {
                worst.push(total.max);
            }
            next_report += 1;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let stats = client.stats();
    let (forwarded, dropped) = {
        let relayed = relayed.lock().unwrap();
        (relayed.forwarded, relayed.dropped)
    };
    worst.sort_unstable();
    println!(
        "TOTAL lost {} skipped {} repaired {}; relay forwarded {} dropped {} ({:.2}%); worst window max latency {:.1} ms",
        stats.frames_lost,
        stats.frames_skipped,
        stats.frames_recovered,
        forwarded,
        dropped,
        100.0 * dropped as f64 / (forwarded + dropped).max(1) as f64,
        worst.last().copied().unwrap_or(0) as f64 / 1000.0
    );

    client.disconnect();
    host.shutdown().await;
    client_ep.close().await;
    host_ep.close().await;
}

/// A frame from a host trace: when it went out, in microseconds, its bytes and datagrams.
fn trace_frames(path: &str) -> Vec<(u64, usize, usize)> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().unwrap().split(',').collect();
    let column = |name: &str| header.iter().position(|h| *h == name).unwrap();
    let (captured, after, bytes, datagrams) =
        (column("capture_time_us"), column("sent_after_us"), column("bytes"), column("datagrams"));
    lines
        .map(|line| {
            let cells: Vec<&str> = line.split(',').collect();
            let at = cells[captured].parse::<u64>().unwrap() + cells[after].parse::<u64>().unwrap();
            (at, cells[bytes].parse().unwrap(), cells[datagrams].parse().unwrap())
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a host trace, and takes as long as the part of it replayed"]
async fn replaying_a_session() {
    let trace = trace_frames(&std::env::var("REPLAY_FRAMES").expect("REPLAY_FRAMES names a host trace"));
    let session_start = trace[0].0;
    let seconds = |name: &str, default: u64| session_start + env(name, default) * 1_000_000;
    let (from, to) = (seconds("REPLAY_FROM", 0), seconds("REPLAY_TO", 60));
    let scale = env("REPLAY_SCALE", 1.0f64);
    let frames: Vec<_> = trace
        .into_iter()
        .filter(|frame| (from..to).contains(&frame.0))
        .map(|(at, bytes, datagrams)| {
            let scaled = |n: usize| ((n as f64 * scale) as usize).max(1);
            (at, scaled(bytes), scaled(datagrams))
        })
        .collect();

    let path = Path {
        bits_per_second: env("REPLAY_MBPS", 80.0) * 1e6,
        queue_bytes: 1024 * 1024,
        delay: Duration::from_millis(env("REPLAY_DELAY_MS", 20)),
        slows_at: None,
        back_at: None,
        jitter: Duration::from_millis(env("PATH_JITTER_MS", 0)),
        stray_loss: stray_loss_from_env(),
    };
    let sender = net::bind_local_endpoint_at(SecretKey::generate(), "[::1]:0").await.unwrap();
    let relay = bottleneck_relay(*sender.bound_sockets().first().unwrap(), path, Arc::default()).await;
    let receiver = net::bind_local_endpoint(SecretKey::generate()).await.unwrap();
    let addr = EndpointAddr::new(sender.id()).with_ip_addr(relay);
    let accepting = sender.clone();
    let accepted = tokio::spawn(async move { accepting.accept().await.unwrap().await.unwrap() });
    let client = net::connect(&receiver, addr).await.unwrap();
    let conn = accepted.await.unwrap();

    // Input the other way, stamped with when it was written, read as the host reads it.
    let clock = Instant::now();
    let input_ms = Arc::new(Mutex::new(Vec::new()));
    let input_hz = env("REPLAY_INPUT_HZ", 0.0f64);
    if input_hz > 0.0 {
        let mut stream = client.open_uni().await.unwrap();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / input_hz));
            loop {
                tick.tick().await;
                let at = clock.elapsed().as_micros() as u64;
                if stream.write_all(&at.to_le_bytes()).await.is_err() {
                    return;
                }
            }
        });
        let (host_side, recording) = (conn.clone(), input_ms.clone());
        tokio::spawn(async move {
            let mut stream = host_side.accept_uni().await.unwrap();
            let mut at = [0; 8];
            while stream.read_exact(&mut at).await.is_ok() {
                let took = clock.elapsed().as_micros() as u64 - u64::from_le_bytes(at);
                recording.lock().unwrap().push(took as f64 / 1000.0 - path.delay.as_secs_f64() * 1000.0);
            }
        });
    }

    // Each datagram starts with its frame's number; the last to arrive says when a frame was whole.
    let arrived = Arc::new(Mutex::new(vec![(None::<Instant>, 0usize); frames.len()]));
    let recording = arrived.clone();
    tokio::spawn(async move {
        while let Ok(datagram) = client.read_datagram().await {
            let frame = u32::from_le_bytes(datagram[..4].try_into().unwrap()) as usize;
            let mut arrived = recording.lock().unwrap();
            arrived[frame] = (Some(Instant::now()), arrived[frame].1 + 1);
        }
    });

    // As while the host is asked to allow the connection.
    tokio::time::sleep(Duration::from_millis(env("REPLAY_IDLE_MS", 0))).await;
    let started = Instant::now();
    let mut sent_at = Vec::with_capacity(frames.len());
    let (mut lowest_estimate, mut lowest_window, mut checked) = (u64::MAX, u64::MAX, 0);
    for (number, &(at, bytes, datagrams)) in frames.iter().enumerate() {
        tokio::time::sleep_until((started + Duration::from_micros(at - frames[0].0)).into()).await;
        sent_at.push(Instant::now());
        let size = (bytes / datagrams.max(1)).clamp(8, 1150);
        for _ in 0..datagrams {
            let mut datagram = vec![0; size];
            datagram[..4].copy_from_slice(&(number as u32).to_le_bytes());
            let _ = conn.send_datagram(datagram.into());
        }
        if started.elapsed().as_secs() > checked {
            checked = started.elapsed().as_secs();
            let snapshot = net::path_snapshot(&conn).unwrap();
            lowest_estimate = lowest_estimate.min(snapshot.bandwidth_estimate.unwrap_or(0));
            lowest_window = lowest_window.min(snapshot.cwnd);
        }
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    let (mut times, mut incomplete) = (Vec::new(), 0);
    for (number, &(last, count)) in arrived.lock().unwrap().iter().enumerate() {
        incomplete += usize::from(count < frames[number].2);
        // The first seconds are BBR finding the path, which a real session did before this part.
        if let Some(last) = last.filter(|_| frames[number].0 - frames[0].0 > 3_000_000) {
            times.push((last - sent_at[number]).as_secs_f64() * 1000.0);
        }
    }
    times.sort_unstable_by(f64::total_cmp);
    let at = |share: f64| times[((times.len() - 1) as f64 * share) as usize];
    println!(
        "{} frames, {incomplete} incomplete; to the last datagram: p50 {:.1} p99 {:.1} p99.9 {:.1} max {:.1} ms; {} over 60 ms",
        frames.len(),
        at(0.5),
        at(0.99),
        at(0.999),
        at(1.0),
        times.iter().filter(|ms| **ms > 60.0).count()
    );
    println!("lowest estimate {:.1} Mb/s, lowest window {} KB", lowest_estimate as f64 / 1e6, lowest_window / 1024);
    let mut input = input_ms.lock().unwrap().clone();
    if !input.is_empty() {
        input.sort_unstable_by(f64::total_cmp);
        let at = |share: f64| input[((input.len() - 1) as f64 * share) as usize];
        println!(
            "{} inputs, ms over the path's delay: p10 {:.2} p50 {:.2} p90 {:.2} p99 {:.2} max {:.2}",
            input.len(),
            at(0.1),
            at(0.5),
            at(0.9),
            at(0.99),
            at(1.0)
        );
    }

    conn.close(0u32.into(), b"done");
    sender.close().await;
    receiver.close().await;
}
