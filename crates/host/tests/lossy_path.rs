//! Video over a bad path: a relay between host and client that drops and reorders packets.
//!
//! `cargo test -p connect-host --release --test lossy_path -- --ignored --nocapture`

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
use tokio::{net::UdpSocket, time::timeout};

/// Forwards UDP between the client and `host`, dropping `loss` of the packets each way and
/// holding back about one in fifty by a couple of milliseconds, so they arrive out of order.
async fn lossy_relay(host: SocketAddr, loss: f64) -> SocketAddr {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = socket.local_addr().unwrap();
    let client: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    tokio::spawn(async move {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut buf = vec![0; 65_536];
        loop {
            let Ok((len, from)) = socket.recv_from(&mut buf).await else { return };
            let to = if from == host {
                match *client.lock().unwrap() {
                    Some(client) => client,
                    None => continue,
                }
            } else {
                *client.lock().unwrap() = Some(from);
                host
            };
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            if (seed % 10_000) as f64 / 10_000.0 < loss {
                continue;
            }
            let packet = buf[..len].to_vec();
            if seed % 50 == 1 {
                let socket = socket.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    let _ = socket.send_to(&packet, to).await;
                });
            } else {
                let _ = socket.send_to(&packet, to).await;
            }
        }
    });
    addr
}

async fn session_over(loss: f64) -> (Stats, u64) {
    let host_ep = net::bind_local_endpoint(SecretKey::generate()).await.unwrap();
    let config = HostConfig {
        video: VideoSource::TestPattern { width: 1280, height: 720, fps: 120 },
        ..HostConfig::new("Lossy host")
    };
    let (host, mut host_events) = connect_host::start(host_ep.clone(), config);
    let host_addr = *host_ep.bound_sockets().first().unwrap();
    let relay = lossy_relay(host_addr, loss).await;

    let client_ep = net::bind_local_endpoint(SecretKey::generate()).await.unwrap();
    let addr = EndpointAddr::new(host_ep.id()).with_ip_addr(relay);
    let (client, mut events) = connect_client::connect(client_ep.clone(), addr, "Lossy client", || {});
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
    // Count what a viewer would see, as the GUI does: take each new frame as it comes.
    let (mut shown, started) = (0u64, Instant::now());
    while started.elapsed() < Duration::from_secs(8) {
        if let Some(frame) = client.take_frame() {
            shown += 1;
            client.recycle(frame);
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let stats = client.stats();
    client.disconnect();
    host.shutdown().await;
    client_ep.close().await;
    host_ep.close().await;
    (stats, shown)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "eight seconds of video per loss rate"]
async fn video_survives_a_lossy_path() {
    for loss in [0.0, 0.01, 0.03] {
        let (stats, shown) = session_over(loss).await;
        println!(
            "{:.0}% loss: shown {shown} frames in 8 s; lost {}, repaired {}; host target {:?} Mb/s, \
             measured loss {:?}",
            loss * 100.0,
            stats.frames_lost,
            stats.frames_recovered,
            stats.host_mbps,
            stats.loss
        );
        if loss > 0.0 {
            assert!(stats.frames_recovered > 0, "the relay should have cost some chunks");
        }
        if loss <= 0.01 {
            assert!(stats.frames_lost <= 2, "{} frames lost at {loss}", stats.frames_lost);
        }
    }
}
