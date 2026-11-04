//! The viewer's trace files reaching the host during a session. A test binary of its own, because
//! the trace folder is set once per process.

use std::{path::Path, time::Duration};

use connect_client::ClientEvent;
use connect_host::{Decision, HostConfig, HostEvent, VideoSource};
use connect_net as net;
use connect_proto::Permissions;
use iroh::SecretKey;
use tokio::time::{Instant, sleep, timeout};

const WAIT: Duration = Duration::from_secs(30);

/// How many rows the newest `<role>-…-frames.csv` in `dir` has, not counting `not_session`.
fn rows(dir: &Path, role: &str, not_session: Option<&str>) -> Option<(String, usize)> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let name = entry.file_name().into_string().ok()?;
        let stamp = name.strip_prefix(role)?.strip_suffix("-frames.csv")?.to_owned();
        (not_session != Some(stamp.as_str()))
            .then(|| (stamp, std::fs::read_to_string(entry.path()).unwrap_or_default().lines().count()))
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewers_files_reach_the_host_even_if_it_sends_no_input() {
    let dir = std::env::temp_dir().join(format!("connect-viewer-files-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(connect_perf::set_trace_dir(dir.clone()));

    let host_ep = net::bind_local_endpoint(SecretKey::generate()).await.unwrap();
    let config = HostConfig {
        video: VideoSource::TestPattern { width: 320, height: 240, fps: 30 },
        ..HostConfig::new("Test host")
    };
    let (_host, mut host_events) = connect_host::start(host_ep.clone(), config);
    let client_ep = net::bind_local_endpoint(SecretKey::generate()).await.unwrap();
    let (client, mut client_events) =
        connect_client::connect(client_ep.clone(), net::local_addr(&host_ep), "Test client", || {});

    let Some(HostEvent::Request(request)) = timeout(WAIT, host_events.recv()).await.unwrap() else {
        panic!("expected a connection request");
    };
    // Sessions are named by the second they start in. Here both sides share a folder, where the
    // forwarded files would land on the viewer's own if both started in the same one.
    sleep(Duration::from_millis(1100)).await;
    request.respond(Decision::Allow(Permissions::VIEW_ONLY));
    loop {
        match timeout(WAIT, client_events.recv()).await.unwrap().expect("client events") {
            ClientEvent::Accepted { .. } => break,
            ClientEvent::Ended(reason) => panic!("ended early: {reason}"),
            _ => {}
        }
    }

    let host_stamp = async {
        loop {
            if let Some((stamp, _)) = rows(&dir, "host-", None) {
                return stamp;
            }
            sleep(Duration::from_millis(100)).await;
        }
    };
    let host_stamp = timeout(WAIT, host_stamp).await.expect("the host's own files");
    let forwarded = dir.join(format!("viewer-{host_stamp}-frames.csv"));
    let deadline = Instant::now() + WAIT;
    let mut lines = 0;
    while Instant::now() < deadline && lines < 10 {
        client.take_frame();
        lines = std::fs::read_to_string(&forwarded).unwrap_or_default().lines().count();
        sleep(Duration::from_millis(100)).await;
    }
    assert!(lines >= 10, "only {lines} lines arrived in {}", forwarded.display());
    let session_file = dir.join(format!("viewer-{host_stamp}-session.csv"));
    let mut session = String::new();
    while Instant::now() < deadline && !session.contains("decoder,") {
        session = std::fs::read_to_string(&session_file).unwrap_or_default();
        sleep(Duration::from_millis(100)).await;
    }
    assert!(session.contains("os,") && session.contains("decoder,"), "{session}");

    client.disconnect();
    client_ep.close().await;
    host_ep.close().await;
    std::fs::remove_dir_all(&dir).unwrap();
}
