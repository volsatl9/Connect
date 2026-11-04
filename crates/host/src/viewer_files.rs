//! Keeps the viewer's trace files with the host's own, so one PC has both sides of a session.

use std::{
    collections::{HashMap, hash_map::Entry},
    fs::File,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use connect_net as net;
use connect_proto::perf::{TraceChunk, TraceFile};
use iroh::endpoint::{Connection, RecvStream};
use tokio::sync::oneshot;
use tracing::{debug, warn};

/// The most kept of any one file. Over an hour at 240 fps comes to about 150 MB; this only stops
/// a viewer filling the disk.
const MOST_KEPT: u64 = 1 << 30;

/// Receives the files until the session ends. `input_taken` says the input stream, which comes
/// first, has been accepted. A viewer from before 0.10.1 sends nothing, and this just waits.
pub(crate) async fn collect(conn: Connection, input_taken: oneshot::Receiver<()>, dir: PathBuf, session: u64) {
    if input_taken.await.is_err() {
        return;
    }
    let recv = match conn.accept_uni().await {
        Ok(recv) => recv,
        Err(e) => return debug!("no trace stream: {e}"),
    };
    receive(recv, dir, session).await;
}

async fn receive(mut recv: RecvStream, dir: PathBuf, session: u64) {
    let dir = std::sync::Arc::new(dir);
    let mut open = HashMap::new();
    loop {
        let chunk = match net::read_msg::<TraceChunk>(&mut recv).await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return,
            Err(e) => return debug!("trace stream ended: {e:#}"),
        };
        let path = dir.clone();
        let wrote = tokio::task::spawn_blocking(move || {
            let result = store(&mut open, &path, session, &chunk);
            (open, result)
        });
        match wrote.await {
            Ok((files, Ok(()))) => open = files,
            Ok((_, Err(e))) => return warn!("couldn't keep the viewer's trace: {e}"),
            Err(_) => return,
        }
    }
}

fn store(open: &mut HashMap<TraceFile, File>, dir: &Path, session: u64, chunk: &TraceChunk) -> std::io::Result<()> {
    let end = chunk.offset.saturating_add(chunk.data.len() as u64);
    if end > MOST_KEPT {
        return Ok(());
    }
    let file = match open.entry(chunk.file) {
        Entry::Occupied(file) => file.into_mut(),
        Entry::Vacant(place) => {
            std::fs::create_dir_all(dir)?;
            place.insert(File::create(connect_perf::trace_path(dir, "viewer", session, chunk.file.name()))?)
        }
    };
    file.seek(SeekFrom::Start(chunk.offset))?;
    file.write_all(&chunk.data)?;
    if chunk.file == TraceFile::Session {
        file.set_len(end)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(file: TraceFile, offset: u64, data: &[u8]) -> TraceChunk {
        TraceChunk { file, offset, data: data.to_vec() }
    }

    #[test]
    fn appends_grow_and_the_session_file_is_replaced() {
        let dir = std::env::temp_dir().join(format!("connect-viewer-files-{}", std::process::id()));
        let mut open = HashMap::new();
        store(&mut open, &dir, 7, &chunk(TraceFile::Frames, 0, b"seq\n")).unwrap();
        store(&mut open, &dir, 7, &chunk(TraceFile::Frames, 4, b"1\n")).unwrap();
        store(&mut open, &dir, 7, &chunk(TraceFile::Session, 0, b"a long summary")).unwrap();
        store(&mut open, &dir, 7, &chunk(TraceFile::Session, 0, b"short")).unwrap();
        assert_eq!(std::fs::read(dir.join("viewer-7-frames.csv")).unwrap(), b"seq\n1\n");
        assert_eq!(std::fs::read(dir.join("viewer-7-session.csv")).unwrap(), b"short");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn past_the_limit_is_ignored() {
        let dir = std::env::temp_dir().join(format!("connect-viewer-limit-{}", std::process::id()));
        let mut open = HashMap::new();
        store(&mut open, &dir, 7, &chunk(TraceFile::Loss, u64::MAX, b"x")).unwrap();
        store(&mut open, &dir, 7, &chunk(TraceFile::Loss, MOST_KEPT, b"x")).unwrap();
        assert!(open.is_empty() && !dir.exists());
    }
}
