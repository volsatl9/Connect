//! Sends this session's trace files to the host as they grow, so both sides' files end up on one
//! PC. They go at the lowest priority and slowly: the viewer's sending is what carries its input.

use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

use connect_net as net;
use connect_proto::perf::{TraceChunk, TraceFile};
use iroh::endpoint::SendStream;
use tokio::time::sleep;
use tracing::debug;

/// One message's worth, sent no more often than [`PACE`]: 160 KB/s, about 1.3 Mb/s, several times
/// what a session at 240 fps writes.
const CHUNK: usize = 16 * 1024;
const PACE: Duration = Duration::from_millis(100);
/// How often to look for more once everything's sent.
const LOOK_EVERY: Duration = Duration::from_secs(1);

/// Runs until the session ends.
pub(crate) async fn forward<const N: usize>(mut stream: SendStream, files: [(TraceFile, PathBuf); N]) {
    if let Err(e) = stream.set_priority(-1) {
        return debug!("trace stream closed: {e}");
    }
    let mut sent = [0u64; N];
    let mut last_session = Vec::new();
    loop {
        let mut more = false;
        for (i, (file, path)) in files.iter().enumerate() {
            let rewritten = *file == TraceFile::Session;
            let path = path.clone();
            let offset = if rewritten { 0 } else { sent[i] };
            let read = tokio::task::spawn_blocking(move || read_from(&path, offset));
            let Ok(data) = read.await else { return };
            if data.is_empty() || (rewritten && data == last_session) {
                continue;
            }
            let chunk = TraceChunk { file: *file, offset, data };
            if let Err(e) = net::write_msg(&mut stream, &chunk).await {
                return debug!("trace stream closed: {e:#}");
            }
            if rewritten {
                last_session = chunk.data;
            } else {
                sent[i] += chunk.data.len() as u64;
                more |= chunk.data.len() == CHUNK;
            }
            sleep(PACE).await;
        }
        if !more {
            sleep(LOOK_EVERY).await;
        }
    }
}

/// Up to a chunk of `path` from `offset`. Nothing if the file isn't there yet.
fn read_from(path: &Path, offset: u64) -> Vec<u8> {
    let read = || -> std::io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut data = Vec::new();
        file.take(CHUNK as u64).read_to_end(&mut data)?;
        Ok(data)
    };
    read().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_on_from_where_it_got_to() {
        let dir = std::env::temp_dir().join(format!("connect-forward-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace.csv");
        assert!(read_from(&path, 0).is_empty(), "not there yet");
        std::fs::write(&path, vec![7u8; CHUNK + 5]).unwrap();
        assert_eq!(read_from(&path, 0).len(), CHUNK);
        assert_eq!(read_from(&path, CHUNK as u64).len(), 5);
        assert!(read_from(&path, CHUNK as u64 + 5).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
