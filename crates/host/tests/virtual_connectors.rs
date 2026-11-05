//! Several connectors on one host, all on this PC: each its own endpoint, stream and decoder, as
//! far as anything but real networks and graphics cards can show.

use std::{path::PathBuf, sync::Once, time::Duration};

use connect_client::{ClientEvent, ClientHandle, EndReason};
use connect_host::{Decision, HostConfig, HostEvent, HostHandle, InputSink, VideoSource};
use connect_net::{self as net, iroh::Endpoint};
use connect_proto::{DeviceClass, Permissions, input::InputEvent};
use iroh::SecretKey;
use tokio::{sync::mpsc, time::timeout};

const WAIT: Duration = Duration::from_secs(30);

struct Connector {
    _endpoint: Endpoint,
    handle: ClientHandle,
    events: mpsc::Receiver<ClientEvent>,
}

fn config() -> HostConfig {
    HostConfig { video: VideoSource::TestPattern { width: 320, height: 240, fps: 30 }, ..HostConfig::new("Host") }
}

async fn start_host(config: HostConfig) -> (Endpoint, HostHandle, mpsc::Receiver<HostEvent>) {
    let endpoint = net::bind_local_endpoint(SecretKey::generate()).await.unwrap();
    let (handle, events) = connect_host::start(endpoint.clone(), config);
    (endpoint, handle, events)
}

async fn connect(host: &Endpoint, name: &str) -> Connector {
    let endpoint = net::bind_local_endpoint(SecretKey::generate()).await.unwrap();
    let (handle, events) = connect_client::connect(endpoint.clone(), net::local_addr(host), name, || {});
    Connector { _endpoint: endpoint, handle, events }
}

/// Connects `name`, the host user allowing it with `permissions`.
async fn join(
    host: &Endpoint,
    host_events: &mut mpsc::Receiver<HostEvent>,
    name: &str,
    permissions: Permissions,
) -> Connector {
    let mut connector = connect(host, name).await;
    loop {
        match timeout(WAIT, host_events.recv()).await.expect("no request").expect("host gone") {
            HostEvent::Request(request) => {
                assert_eq!(request.client_name, name);
                request.respond(Decision::Allow(permissions));
            }
            HostEvent::SessionStarted { client_name, .. } if client_name == name => break,
            _ => {}
        }
    }
    loop {
        match timeout(WAIT, connector.events.recv()).await.expect("no answer").expect("client gone") {
            ClientEvent::Accepted { .. } => return connector,
            ClientEvent::Ended(reason) => panic!("{name} ended: {reason}"),
            _ => {}
        }
    }
}

/// Waits for a fresh decoded picture.
async fn sees_video(connector: &Connector) {
    timeout(WAIT, async {
        loop {
            if connector.handle.take_frame().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("no video");
}

#[tokio::test(flavor = "multi_thread")]
async fn eight_connectors_each_get_their_own_stream() {
    let (host_endpoint, host, mut events) = start_host(config()).await;
    let mut connectors = Vec::new();
    for number in 1..=8 {
        connectors.push(join(&host_endpoint, &mut events, &format!("PC {number}"), Permissions::ALL).await);
    }
    for connector in &connectors {
        sees_video(connector).await;
    }
    let sessions = host.sessions();
    assert_eq!(sessions.len(), 8);
    assert_eq!(sessions.iter().map(|s| s.player).collect::<Vec<_>>(), (1..=8).collect::<Vec<_>>());
    assert_eq!(sessions[2].client_name, "PC 3");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_host_sets_how_many_may_connect() {
    let (host_endpoint, host, mut events) = start_host(HostConfig { max_connections: 2, ..config() }).await;
    let _first = join(&host_endpoint, &mut events, "First", Permissions::ALL).await;
    let _second = join(&host_endpoint, &mut events, "Second", Permissions::ALL).await;
    let mut third = connect(&host_endpoint, "Third").await;
    let turned_away = timeout(WAIT, async {
        loop {
            if let Some(ClientEvent::Ended(reason)) = third.events.recv().await {
                return reason;
            }
        }
    })
    .await
    .unwrap();
    assert!(matches!(&turned_away, EndReason::Rejected(reason) if reason.contains("C-105")), "{turned_away:?}");

    host.set_max_connections(3);
    let third = join(&host_endpoint, &mut events, "Fourth", Permissions::ALL).await;
    sees_video(&third).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_one_leaves_the_others_playing() {
    let (host_endpoint, host, mut events) = start_host(config()).await;
    let mut first = join(&host_endpoint, &mut events, "First", Permissions::ALL).await;
    let second = join(&host_endpoint, &mut events, "Second", Permissions::ALL).await;
    let third = join(&host_endpoint, &mut events, "Third", Permissions::ALL).await;
    let dropped = host.sessions()[0].id;
    host.disconnect_session(dropped);
    let ended = timeout(WAIT, async {
        loop {
            if let Some(ClientEvent::Ended(reason)) = first.events.recv().await {
                return reason;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(ended, EndReason::HostEnded("The host ended the session.".into()));

    for _ in 0..30 {
        sees_video(&second).await;
        sees_video(&third).await;
    }
    let left = host.sessions();
    assert_eq!(left.iter().map(|s| s.client_name.as_str()).collect::<Vec<_>>(), ["Second", "Third"]);
    assert_eq!(left.iter().map(|s| s.player).collect::<Vec<_>>(), [2, 3], "players keep their numbers");
    // The freed number goes to whoever joins next.
    let _fourth = join(&host_endpoint, &mut events, "Fourth", Permissions::ALL).await;
    assert_eq!(host.sessions().last().unwrap().player, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn permissions_are_each_connectors_own() {
    let (input, mut received) = mpsc::unbounded_channel();
    let config = HostConfig { input: InputSink::Channel(input), ..config() };
    let (host_endpoint, host, mut events) = start_host(config).await;
    let view_only = Permissions::VIEW_ONLY;
    let keys = view_only.with(DeviceClass::Keyboard, true);
    let viewer = join(&host_endpoint, &mut events, "Viewer", view_only).await;
    let typist = join(&host_endpoint, &mut events, "Typist", keys).await;

    viewer.handle.send_input(InputEvent::Key { scancode: 30, down: true });
    typist.handle.send_input(InputEvent::Key { scancode: 31, down: true });
    let event = timeout(WAIT, received.recv()).await.unwrap().unwrap();
    assert_eq!(event, InputEvent::Key { scancode: 31, down: true });

    host.set_session_permissions(host.sessions()[0].id, keys);
    host.set_session_permissions(host.sessions()[1].id, view_only);
    // Taking the keyboard away releases the key the typist held.
    let event = timeout(WAIT, received.recv()).await.unwrap().unwrap();
    assert_eq!(event, InputEvent::Key { scancode: 31, down: false });
    tokio::time::sleep(Duration::from_millis(200)).await;
    typist.handle.send_input(InputEvent::Key { scancode: 32, down: true });
    viewer.handle.send_input(InputEvent::Key { scancode: 33, down: true });
    let event = timeout(WAIT, received.recv()).await.unwrap().unwrap();
    assert_eq!(event, InputEvent::Key { scancode: 33, down: true });

    host.disallow_all(DeviceClass::Keyboard);
    let event = timeout(WAIT, received.recv()).await.unwrap().unwrap();
    assert_eq!(event, InputEvent::Key { scancode: 33, down: false });
    assert!(host.sessions().iter().all(|s| !s.permissions.allows(DeviceClass::Keyboard)));
}

#[tokio::test(flavor = "multi_thread")]
async fn each_connector_sees_the_others_pointers_by_name() {
    let (host_endpoint, _host, mut events) = start_host(config()).await;
    let alice = join(&host_endpoint, &mut events, "Alice's PC", Permissions::ALL).await;
    let bob = join(&host_endpoint, &mut events, "Bob's PC", Permissions::ALL).await;
    alice.handle.send_cursor(1000, 2000, true);
    let pointers = timeout(WAIT, async {
        loop {
            let pointers = bob.handle.pointers();
            if !pointers.is_empty() {
                return pointers;
            }
            alice.handle.send_cursor(1000, 2000, true);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!((pointers[0].name.as_str(), pointers[0].x, pointers[0].y), ("Alice's PC", 1000, 2000));
    assert!(alice.handle.pointers().is_empty(), "nobody sees their own");
}

fn trace_dir() -> PathBuf {
    static SET: Once = Once::new();
    let dir = std::env::temp_dir().join(format!("connect-virtual-{}", std::process::id()));
    SET.call_once(|| {
        std::fs::create_dir_all(&dir).unwrap();
        connect_perf::set_trace_dir(dir.clone());
    });
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn every_connectors_logs_reach_the_host_with_latency_numbers() {
    let dir = trace_dir();
    let (host_endpoint, host, mut events) = start_host(config()).await;
    let first = join(&host_endpoint, &mut events, "First", Permissions::ALL).await;
    let second = join(&host_endpoint, &mut events, "Second", Permissions::ALL).await;
    sees_video(&first).await;
    sees_video(&second).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    first.handle.disconnect();
    second.handle.disconnect();
    let mut stamps = Vec::new();
    while stamps.len() < 2 {
        if let HostEvent::SessionEnded { stamp, .. } = timeout(WAIT, events.recv()).await.unwrap().unwrap() {
            stamps.push(stamp);
        }
    }
    assert_ne!(stamps[0], stamps[1], "each session has its own files");
    host.set_accepting(false);
    for stamp in stamps {
        for what in ["host-{}-frames.csv", "host-{}-network.csv", "viewer-{}-frames.csv", "viewer-{}-network.csv"] {
            let path = dir.join(what.replace("{}", &stamp.to_string()));
            let lines = timeout(WAIT, async {
                loop {
                    let lines = std::fs::read_to_string(&path).map_or(0, |text| text.lines().count());
                    if lines > 1 {
                        return lines;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("nothing in {}", path.display()));
            assert!(lines > 1);
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}
