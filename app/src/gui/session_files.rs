//! The folder sessions leave their trace files in, while latency and stutter are being tracked
//! down. Hosting brings the viewer's files here too, so one PC has both sides of a session and
//! its report. Turning this off is one call: without [`start`], nothing is kept.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use tracing::{debug, warn};

/// Sessions kept, of hosting and of viewing each.
const KEEP: usize = 5;

pub fn folder(config_dir: &Path) -> PathBuf {
    config_dir.join("session-files")
}

/// Keeps sessions' files in the folder, unless `CONNECT_TRACE` says otherwise.
pub fn start(config_dir: &Path) {
    if std::env::var_os("CONNECT_TRACE").is_some() {
        return;
    }
    let folder = folder(config_dir);
    prune(&folder);
    connect_perf::set_trace_dir(folder);
}

/// Follows every frame through the session just hosted, and makes room for the next.
pub fn session_ended() {
    let Some(folder) = connect_perf::trace_dir() else { return };
    let spawned = std::thread::Builder::new().name("connect-session-report".into()).spawn(move || {
        match crate::trace_report::write(&folder) {
            Ok((_, path)) => tracing::info!("session report: {}", path.display()),
            // A viewer from before 0.10.1 leaves only the host's files.
            Err(e) => debug!("no session report: {e:#}"),
        }
        prune(&folder);
    });
    if let Err(e) = spawned {
        warn!("couldn't start the session report: {e}");
    }
}

/// Shows the folder in the file manager.
pub fn open(folder: &Path) {
    let opener = if cfg!(windows) { "explorer" } else { "xdg-open" };
    if let Err(e) = std::fs::create_dir_all(folder).and_then(|()| Command::new(opener).arg(folder).spawn().map(drop)) {
        warn!("couldn't open {}: {e}", folder.display());
    }
}

/// `host-1700000000-frames.csv` → ("host", 1700000000). Reports go with their host session.
fn session_of(name: &str) -> Option<(&str, u64)> {
    let (role, rest) = name.split_once('-')?;
    let role = match role {
        "host" | "report" => "host",
        "viewer" => "viewer",
        _ => return None,
    };
    let stamp = rest.split(['-', '.']).next()?;
    Some((role, stamp.parse().ok()?))
}

/// Deletes the session files in `folder` beyond the newest [`KEEP`] of each role.
fn prune(folder: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    let files: Vec<(PathBuf, (String, u64))> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let (role, stamp) = session_of(name.to_str()?)?;
            Some((entry.path(), (role.to_owned(), stamp)))
        })
        .collect();
    for role in ["host", "viewer"] {
        let mut stamps: Vec<u64> = files.iter().filter(|(_, (r, _))| r == role).map(|(_, (_, s))| *s).collect();
        stamps.sort_unstable_by_key(|s| std::cmp::Reverse(*s));
        stamps.dedup();
        for old in stamps.into_iter().skip(KEEP) {
            for (path, _) in files.iter().filter(|(_, (r, s))| r == role && *s == old) {
                if let Err(e) = std::fs::remove_file(path) {
                    debug!("couldn't delete {}: {e}", path.display());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_read_by_session() {
        assert_eq!(session_of("host-17-frames.csv"), Some(("host", 17)));
        assert_eq!(session_of("viewer-17-save2-loss.csv"), Some(("viewer", 17)));
        assert_eq!(session_of("report-17.txt"), Some(("host", 17)));
        assert_eq!(session_of("report-17-events.csv"), Some(("host", 17)));
        assert_eq!(session_of("crash-17.txt"), None);
        assert_eq!(session_of("connect.log"), None);
    }

    #[test]
    fn only_the_newest_sessions_stay() {
        let dir = std::env::temp_dir().join(format!("connect-session-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for stamp in 1..=7 {
            for name in [format!("host-{stamp}-frames.csv"), format!("report-{stamp}.txt")] {
                std::fs::write(dir.join(name), "").unwrap();
            }
        }
        std::fs::write(dir.join("viewer-1-frames.csv"), "").unwrap();
        std::fs::write(dir.join("crash-1.txt"), "").unwrap();
        prune(&dir);
        let stays = |name: &str| dir.join(name).exists();
        assert!(!stays("host-2-frames.csv") && !stays("report-2.txt"));
        assert!(stays("host-3-frames.csv") && stays("report-3.txt") && stays("host-7-frames.csv"));
        assert!(stays("viewer-1-frames.csv"), "viewing and hosting count apart");
        assert!(stays("crash-1.txt"), "a crash is never pruned");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
