//! Host ↔ client over two localhost endpoints: the approval flow, then video and input.

use std::time::Duration;

use connect_client::{ClientEvent, ClientHandle, EndReason, Stage};
use connect_host::{Decision, HostConfig, HostEvent, HostHandle, InputSink, VideoSource};
use connect_net::{self as net, iroh::Endpoint};
use connect_proto::{
    ALPN, DeviceClass, Permissions,
    control::{ClientMsg, HostMsg},
    input::{GamepadState, InputEvent, MouseButton},
    video::FrameAssembler,
};
use iroh::{
    SecretKey,
    endpoint::{Connection, RecvStream, SendStream},
};
use tokio::{sync::mpsc, time::timeout};

const WAIT: Duration = Duration::from_secs(20);

fn config() -> HostConfig {
    HostConfig { video: VideoSource::TestPattern { width: 320, height: 240, fps: 30 }, ..HostConfig::new("Test host") }
}

async fn endpoint() -> Endpoint {
    net::bind_local_endpoint(SecretKey::generate()).await.unwrap()
}

async fn start_host(config: HostConfig) -> (Endpoint, HostHandle, mpsc::Receiver<HostEvent>) {
    let ep = endpoint().await;
    let (handle, events) = connect_host::start(ep.clone(), config);
    (ep, handle, events)
}

async fn start_client(host: &Endpoint) -> (Endpoint, ClientHandle, mpsc::Receiver<ClientEvent>) {
    let ep = endpoint().await;
    let (handle, events) = connect_client::connect(ep.clone(), net::local_addr(host), "Test client", || {});
    (ep, handle, events)
}

async fn next<T>(rx: &mut mpsc::Receiver<T>) -> T {
    timeout(WAIT, rx.recv()).await.expect("timed out waiting for an event").expect("channel closed")
}

async fn request(host: &mut mpsc::Receiver<HostEvent>) -> connect_host::ConnectionRequest {
    match next(host).await {
        HostEvent::Request(request) => request,
        other => panic!("expected a connection request, got {other:?}"),
    }
}

/// Skips progress events and returns how the attempt ended.
async fn ended(client: &mut mpsc::Receiver<ClientEvent>) -> EndReason {
    loop {
        if let ClientEvent::Ended(reason) = next(client).await {
            return reason;
        }
    }
}

async fn accepted(client: &mut mpsc::Receiver<ClientEvent>) -> (String, u32, u32) {
    loop {
        match next(client).await {
            ClientEvent::Accepted { host_name, width, height, .. } => return (host_name, width, height),
            ClientEvent::Ended(reason) => panic!("ended before being accepted: {reason}"),
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn deny_rejects_with_reason() {
    let (host_ep, _host, mut host_events) = start_host(config()).await;
    let (client_ep, _client, mut client_events) = start_client(&host_ep).await;

    assert_eq!(next(&mut client_events).await, ClientEvent::Stage(Stage::Connecting));
    let req = request(&mut host_events).await;
    assert_eq!(req.peer, client_ep.id(), "the prompt shows the verified User-ID");
    assert_eq!(req.client_name, "Test client");
    assert!(req.is_pending());
    assert_eq!(next(&mut client_events).await, ClientEvent::Stage(Stage::WaitingForApproval));
    req.respond(Decision::Deny);

    assert_eq!(ended(&mut client_events).await, EndReason::Rejected("The host declined the connection.".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn unanswered_request_times_out() {
    let (host_ep, _host, mut host_events) =
        start_host(HostConfig { approval_timeout: Duration::from_millis(300), ..config() }).await;
    let (_client_ep, _client, mut client_events) = start_client(&host_ep).await;

    let req = request(&mut host_events).await;
    assert_eq!(ended(&mut client_events).await, EndReason::Rejected("The host didn't answer in time.".into()));
    assert!(!req.is_pending(), "the prompt can tell it expired");
}

#[tokio::test(flavor = "multi_thread")]
async fn allow_streams_video() {
    let (host_ep, _host, mut host_events) = start_host(config()).await;
    let (_client_ep, client, mut client_events) = start_client(&host_ep).await;

    request(&mut host_events).await.respond(Decision::Allow(Permissions::ALL));
    assert_eq!(accepted(&mut client_events).await, ("Test host".into(), 320, 240));
    assert!(matches!(next(&mut host_events).await, HostEvent::SessionStarted { .. }));

    let mut frames = 0;
    timeout(WAIT, async {
        while frames < 10 {
            if let Some(frame) = client.take_frame() {
                assert_eq!((frame.width, frame.height), (320, 240));
                assert_eq!(frame.yuv.len(), 320 * 240 * 3 / 2);
                frames += 1;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("video frames arrive");

    client.disconnect();
    assert_eq!(ended(&mut client_events).await, EndReason::Disconnected);
    match next(&mut host_events).await {
        HostEvent::SessionEnded { reason, .. } => assert_eq!(reason, "Test client disconnected."),
        other => panic!("expected the session to end, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn trusted_peer_needs_no_prompt() {
    let (host_ep, host, mut host_events) = start_host(config()).await;
    let client_ep = endpoint().await;
    host.set_trusted(client_ep.id(), true);
    let (_client, mut client_events) =
        connect_client::connect(client_ep.clone(), net::local_addr(&host_ep), "Trusted", || {});

    accepted(&mut client_events).await;
    assert!(matches!(next(&mut host_events).await, HostEvent::SessionStarted { .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn high_refresh_video_decodes_over_datagrams() {
    for fps in [120, 240] {
        let mut cfg = config();
        cfg.video = VideoSource::TestPattern { width: 640, height: 360, fps };
        let (host_ep, host, mut host_events) = start_host(cfg).await;
        let (_client_ep, client, mut client_events) = start_client(&host_ep).await;
        request(&mut host_events).await.respond(Decision::Allow(Permissions::VIEW_ONLY));
        accepted(&mut client_events).await;

        timeout(WAIT, async {
            let mut previous = None;
            let mut received = 0;
            while received < 20 {
                if let Some(frame) = client.take_frame() {
                    assert_eq!((frame.width, frame.height), (640, 360));
                    assert_eq!(frame.yuv.len(), 640 * 360 * 3 / 2);
                    assert!(previous.is_none_or(|seq| frame.meta.seq > seq));
                    previous = Some(frame.meta.seq);
                    received += 1;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("high-refresh video keeps decoding");

        client.disconnect();
        assert_eq!(ended(&mut client_events).await, EndReason::Disconnected);
        timeout(WAIT, host.shutdown()).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn second_client_is_told_host_is_busy() {
    let (host_ep, host, mut host_events) = start_host(config()).await;
    let (first_ep, _first, mut first_events) = start_client(&host_ep).await;
    request(&mut host_events).await.respond(Decision::Allow(Permissions::ALL));
    accepted(&mut first_events).await;
    assert_eq!(host.current_peer(), Some(first_ep.id()));

    let (_second_ep, _second, mut second_events) = start_client(&host_ep).await;
    assert_eq!(
        ended(&mut second_events).await,
        EndReason::Rejected("The host is in another session right now.".into())
    );

    host.disconnect();
    assert_eq!(ended(&mut first_events).await, EndReason::HostEnded("The host ended the session.".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn not_accepting_turns_everyone_away() {
    let (host_ep, host, _host_events) = start_host(config()).await;
    host.set_accepting(false);
    let (_client_ep, _client, mut client_events) = start_client(&host_ep).await;
    assert_eq!(
        ended(&mut client_events).await,
        EndReason::Rejected("The host isn't accepting connections right now.".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn chosen_permissions_reach_the_client() {
    let (host_ep, host, mut host_events) = start_host(config()).await;
    let (_client_ep, _client, mut client_events) = start_client(&host_ep).await;
    let mouse_only = Permissions::VIEW_ONLY.with(DeviceClass::Mouse, true);
    request(&mut host_events).await.respond(Decision::Allow(mouse_only));

    let granted = loop {
        match next(&mut client_events).await {
            ClientEvent::Accepted { permissions, .. } => break permissions,
            ClientEvent::Ended(reason) => panic!("ended before being accepted: {reason}"),
            _ => {}
        }
    };
    assert_eq!(granted, mouse_only);
    match next(&mut host_events).await {
        HostEvent::SessionStarted { permissions, .. } => assert_eq!(permissions, mouse_only),
        other => panic!("expected the session to start, got {other:?}"),
    }
    assert_eq!(host.permissions(), mouse_only);

    host.set_permissions(Permissions::ALL);
    loop {
        match next(&mut client_events).await {
            ClientEvent::PermissionsChanged(p) => break assert_eq!(p, Permissions::ALL),
            ClientEvent::Ended(reason) => panic!("ended early: {reason}"),
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn input_obeys_permissions_and_is_released() {
    let (sink, mut injected) = mpsc::unbounded_channel();
    let (host_ep, host, mut host_events) = start_host(HostConfig { input: InputSink::Channel(sink), ..config() }).await;
    let (_client_ep, client, mut client_events) = start_client(&host_ep).await;
    let no_gamepad = Permissions::ALL.with(DeviceClass::Gamepad, false);
    request(&mut host_events).await.respond(Decision::Allow(no_gamepad));
    accepted(&mut client_events).await;

    let w_down = InputEvent::Key { scancode: 0x11, down: true };
    let click = InputEvent::MouseButton { button: MouseButton::Left, down: true };
    let pad = GamepadState { buttons: GamepadState::A, ..GamepadState::NEUTRAL };
    client.send_input(w_down);
    client.send_input(InputEvent::Gamepad { pad: 0, state: pad }); // gamepad not allowed
    client.send_input(InputEvent::Key { scancode: 0, down: true }); // malformed
    client.send_input(click);
    let mut injected_next = async || timeout(WAIT, injected.recv()).await.expect("input arrives").unwrap();
    assert_eq!(injected_next().await, w_down);
    assert_eq!(injected_next().await, click);

    // Turning the keyboard off lets go of W straight away, and stops further keys.
    host.set_permissions(no_gamepad.with(DeviceClass::Keyboard, false));
    assert_eq!(injected_next().await, InputEvent::Key { scancode: 0x11, down: false });
    client.send_input(InputEvent::Key { scancode: 0x1E, down: true });

    // Ending the session lets go of the mouse button.
    client.disconnect();
    assert_eq!(injected_next().await, InputEvent::MouseButton { button: MouseButton::Left, down: false });
    let extra = timeout(Duration::from_millis(300), injected.recv()).await;
    assert!(!matches!(extra, Ok(Some(_))), "unexpected input: {extra:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_releases_input_before_returning() {
    let (sink, mut injected) = mpsc::unbounded_channel();
    let (host_ep, host, mut host_events) = start_host(HostConfig { input: InputSink::Channel(sink), ..config() }).await;
    let (_client_ep, client, mut client_events) = start_client(&host_ep).await;
    request(&mut host_events).await.respond(Decision::Allow(Permissions::ALL));
    accepted(&mut client_events).await;

    let space = InputEvent::Key { scancode: 0x39, down: true };
    client.send_input(space);
    assert_eq!(timeout(WAIT, injected.recv()).await.unwrap().unwrap(), space);

    timeout(WAIT, host.shutdown()).await.expect("shutdown finishes");
    assert_eq!(injected.try_recv().ok(), Some(InputEvent::Key { scancode: 0x39, down: false }), "released by then");
    assert_eq!(ended(&mut client_events).await, EndReason::HostEnded("The host ended the session.".into()));
    assert!(!host.accepting());
    timeout(WAIT, host.shutdown()).await.expect("a second shutdown returns at once");
}

#[tokio::test(flavor = "multi_thread")]
async fn rapid_gamepad_edges_and_visual_pointer_preserve_input_permissions() {
    let (sink, mut injected) = mpsc::unbounded_channel();
    let (host_ep, host, mut host_events) = start_host(HostConfig { input: InputSink::Channel(sink), ..config() }).await;
    let (_ep, client, mut events) = start_client(&host_ep).await;
    request(&mut host_events).await.respond(Decision::Allow(Permissions::ALL));
    accepted(&mut events).await;
    let down =
        InputEvent::Gamepad { pad: 0, state: GamepadState { buttons: GamepadState::A, ..GamepadState::NEUTRAL } };
    let up = InputEvent::Gamepad { pad: 0, state: GamepadState::NEUTRAL };
    client.send_cursor(123, 456, true);
    client.send_input(down);
    client.send_input(up);
    assert_eq!(timeout(WAIT, injected.recv()).await.unwrap().unwrap(), down);
    assert_eq!(timeout(WAIT, injected.recv()).await.unwrap().unwrap(), up);
    host.set_permissions(Permissions::VIEW_ONLY);
    loop {
        if matches!(next(&mut events).await, ClientEvent::PermissionsChanged(Permissions::VIEW_ONLY)) {
            break;
        }
    }
    client.send_cursor(500, 600, true);
    assert!(
        timeout(Duration::from_millis(50), injected.recv()).await.is_err(),
        "visual pointer telemetry must never inject mouse input"
    );
    client.disconnect();
    timeout(WAIT, host.shutdown()).await.unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
#[ignore = "captures the local desktop over localhost, requires GPU and audio devices"]
async fn native_desktop_gpu_cursor_audio_and_revocation() {
    let (sink, _injected) = mpsc::unbounded_channel();
    let cfg = HostConfig {
        video: VideoSource::Screen,
        input: InputSink::Channel(sink),
        ..HostConfig::new("Native media probe")
    };
    let (host_ep, host, mut host_events) = start_host(cfg).await;
    let (_client_ep, client, mut client_events) = start_client(&host_ep).await;
    // Same-PC loopback playback must stay silent to avoid recapturing its own output.
    client.set_audio_muted(true);
    request(&mut host_events).await.respond(Decision::Allow(Permissions::VIEW_ONLY));
    accepted(&mut client_events).await;
    let mut gpu = false;
    let mut frames = 0;
    timeout(WAIT, async {
        while !(gpu && frames >= 5 && client.cursor().shape.is_some()) {
            while let Ok(event) = client_events.try_recv() {
                match event {
                    ClientEvent::VideoEncoder { name } => {
                        println!("Native session encoder: {name}");
                        gpu = name.starts_with("GPU:");
                    }
                    ClientEvent::Ended(reason) => panic!("native session ended: {reason}"),
                    _ => {}
                }
            }
            if let Some(frame) = client.take_frame() {
                assert!(frame.width > 0 && frame.height > 0);
                frames += 1;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("GPU desktop video and independent host cursor arrive");
    client.send_cursor(20000, 30000, true);
    host.set_permissions(Permissions::VIEW_ONLY.with(DeviceClass::Audio, true));
    timeout(WAIT, async {
        loop {
            match next(&mut client_events).await {
                ClientEvent::AudioStatus { active: true, .. } => break,
                ClientEvent::AudioStatus { reason: Some(reason), .. } => panic!("audio unavailable: {reason}"),
                ClientEvent::Ended(reason) => panic!("session ended: {reason}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    host.set_permissions(Permissions::VIEW_ONLY);
    timeout(WAIT, async {
        loop {
            if matches!(next(&mut client_events).await, ClientEvent::AudioStatus { active: false, reason: None }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    client.send_cursor(0, 0, false);
    client.disconnect();
    timeout(WAIT, host.shutdown()).await.unwrap();
    println!("Native localhost session passed: GPU video, cursor, audio startup/revocation, clean shutdown");
}

#[cfg(not(windows))]
#[tokio::test(flavor = "multi_thread")]
async fn only_windows_can_host() {
    let (host_ep, _host, _host_events) = start_host(HostConfig::new("Linux box")).await;
    let (_client_ep, _client, mut client_events) = start_client(&host_ep).await;
    match ended(&mut client_events).await {
        EndReason::Rejected(reason) => assert!(reason.contains("hosting needs Windows"), "{reason}"),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn client_giving_up_withdraws_the_prompt() {
    let (host_ep, _host, mut host_events) = start_host(config()).await;
    let (_client_ep, client, mut client_events) = start_client(&host_ep).await;
    let req = request(&mut host_events).await;
    client.disconnect();
    assert_eq!(ended(&mut client_events).await, EndReason::Disconnected);
    timeout(WAIT, async {
        while req.is_pending() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the request stops being pending");
}

/// Talks the protocol directly, for cases the client library never produces.
struct Raw {
    _ep: Endpoint, // the connection dies with its endpoint
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
}

async fn raw_hello(host_ep: &Endpoint, version: u32) -> Raw {
    let ep = endpoint().await;
    let conn = timeout(WAIT, ep.connect(net::local_addr(host_ep), ALPN)).await.unwrap().unwrap();
    let (mut send, recv) = conn.open_bi().await.unwrap();
    net::write_msg(&mut send, &ClientMsg::Hello { version, client_name: "Raw".into() }).await.unwrap();
    Raw { _ep: ep, conn, send, recv }
}

async fn read_reply(recv: &mut RecvStream) -> Option<HostMsg> {
    timeout(WAIT, net::read_msg::<HostMsg>(recv)).await.expect("timed out waiting for the host").unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn version_mismatch_is_rejected() {
    let (host_ep, _host, _host_events) = start_host(config()).await;
    let mut raw = raw_hello(&host_ep, 999).await;
    match read_reply(&mut raw.recv).await {
        Some(HostMsg::Rejected { reason }) => assert!(reason.contains("different version"), "{reason}"),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn keyframe_request_is_honoured() {
    let (host_ep, _host, mut host_events) = start_host(config()).await;
    let Raw { _ep, conn, mut send, mut recv } = raw_hello(&host_ep, connect_proto::PROTOCOL_VERSION).await;
    request(&mut host_events).await.respond(Decision::Allow(Permissions::ALL));
    assert!(matches!(read_reply(&mut recv).await, Some(HostMsg::Welcome { .. })));

    let mut asm = FrameAssembler::new();
    let mut next_frame = async || loop {
        let datagram = timeout(WAIT, conn.read_datagram()).await.unwrap().unwrap();
        asm.push(datagram).unwrap();
        if let Some(frame) = asm.next_frame() {
            return frame;
        }
    };
    let first = next_frame().await;
    assert!(first.meta.keyframe, "the stream starts with a keyframe");
    for _ in 0..3 {
        assert!(!next_frame().await.meta.keyframe, "then delta frames");
    }

    net::write_msg(&mut send, &ClientMsg::RequestKeyframe).await.unwrap();
    let got_keyframe = timeout(WAIT, async {
        loop {
            if next_frame().await.meta.keyframe {
                break;
            }
        }
    })
    .await;
    assert!(got_keyframe.is_ok(), "a keyframe follows the request");
}
