//! Video through a path slower than the host's bitrate cap: a relay between host and client that
//! forwards at a fixed rate through a small queue, dropping packets that find it full, as a home
//! router or an ISP's link does. The source is noise the encoder can't compress, so the video
//! uses all the bitrate it's given, as a busy game does. What matters is whether the stream
//! settles under the path's rate without losing frames, and what its latency does meanwhile.
//!
//! `cargo test -p connect-host --release --test bottleneck -- --ignored --nocapture`
//! (`BOTTLENECK_MBPS`, `BOTTLENECK_QUEUE_KB`, `BOTTLENECK_SECS`, `BOTTLENECK_FPS` change the path
//! and the run; `CONNECT_RATE_CEILING=0` runs 0.9.0's rate controller, to compare.)

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
        loop {
            let Ok((len, _)) = host_side.recv_from(&mut buf).await else { return };
            let Some(client) = *client.lock().unwrap() else { continue };
            let now = tokio::time::Instant::now();
            free_at = free_at.max(now);
            let held = (free_at - now).as_secs_f64() * path.bits_per_second / 8.0;
            if held as usize + len > path.queue_bytes {
                relayed.lock().unwrap().dropped += 1;
                continue;
            }
            free_at += Duration::from_secs_f64(len as f64 * 8.0 / path.bits_per_second);
            relayed.lock().unwrap().forwarded += 1;
            let _ = to_client.send((free_at + path.delay, buf[..len].to_vec(), client));
        }
    });
    addr
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
    };
    let seconds = env("BOTTLENECK_SECS", 30u64);
    let fps = env("BOTTLENECK_FPS", 120u32);

    let host_ep = net::bind_local_endpoint_at(SecretKey::generate(), "[::1]:0").await.unwrap();
    let config = HostConfig {
        video: VideoSource::NoisyTestPattern { width: 1280, height: 720, fps },
        bitrate_mbps: 20,
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
        "path {:.0} Mb/s, queue {} KB, {} ms each way; host cap 20 Mb/s",
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
