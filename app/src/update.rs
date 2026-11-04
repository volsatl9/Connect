//! Updates from GitHub Releases: find a newer build, download it next to the running executable,
//! check it against the release's SHA256SUMS.txt, then swap it in. The running file can't be
//! overwritten on Windows but can be renamed, so it moves aside to `<name>.old` and is deleted by
//! the next start.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{debug, info};

const RELEASES_API: &str = "https://api.github.com/repos/volsatl9/Connect/releases?per_page=20";
const SUMS_FILE: &str = "SHA256SUMS.txt";
#[cfg(windows)]
const ASSET: &str = "connect.exe";
#[cfg(not(windows))]
const ASSET: &str = "connect-linux-x86_64";

/// Pretend to be this version when checking, e.g. `CONNECT_UPDATE_FROM=0.1.0`, to try updating.
const VERSION_OVERRIDE: &str = "CONNECT_UPDATE_FROM";

/// A release newer than the running one, with a build for this platform.
#[derive(Debug, Clone)]
pub struct Update {
    /// e.g. "0.2.1"
    pub version: String,
    pub notes_url: String,
    asset_url: String,
    sums_url: String,
    size: u64,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    draft: bool,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// The running version, as compared against releases.
pub fn current_version() -> String {
    std::env::var(VERSION_OVERRIDE).unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_owned())
}

/// Whether a Linux package put this copy where only the package manager can change it.
pub fn installed_by_package() -> bool {
    std::env::current_exe().is_ok_and(|exe| in_system_folder(&exe))
}

fn in_system_folder(exe: &Path) -> bool {
    cfg!(unix) && exe.starts_with("/usr")
}

/// The newest release above the running version that has a build for this platform.
pub async fn check() -> Result<Option<Update>> {
    let current = parse_version(&current_version()).context("unreadable app version")?;
    let releases: Vec<GhRelease> = client()?
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("couldn't reach GitHub")?
        .error_for_status()
        .context("GitHub turned the update check down")?
        .json()
        .await
        .context("unexpected reply from GitHub")?;

    let newest = releases
        .into_iter()
        .filter(|r| !r.draft)
        .filter_map(|r| {
            let version = parse_version(&r.tag_name)?;
            let asset = r.assets.iter().find(|a| a.name == ASSET)?;
            let sums = r.assets.iter().find(|a| a.name == SUMS_FILE)?;
            let update = Update {
                version: r.tag_name.trim_start_matches('v').to_owned(),
                notes_url: r.html_url.clone(),
                asset_url: asset.browser_download_url.clone(),
                sums_url: sums.browser_download_url.clone(),
                size: asset.size,
            };
            Some((version, update))
        })
        .filter(|(version, _)| *version > current)
        .max_by_key(|(version, _)| *version)
        .map(|(_, update)| update);
    debug!(?newest, "update check done");
    Ok(newest)
}

/// Downloads and verifies `update` into a file beside the running executable, reporting progress
/// in thousandths. Nothing is left behind if it fails.
pub async fn download(update: &Update, progress: Arc<AtomicU32>) -> Result<PathBuf> {
    let client = client()?;
    let sums = client.get(&update.sums_url).send().await?.error_for_status()?.text().await?;
    let expected = expected_hash(&sums, ASSET).context("the release has no checksum for this build")?;

    let staged = staged_path()?;
    let result = async {
        let mut response = client.get(&update.asset_url).send().await?.error_for_status()?;
        let mut file = std::fs::File::create(&staged)
            .with_context(|| format!("can't write next to the app ({})", staged.display()))?;
        let mut hasher = Sha256::new();
        let mut received = 0u64;
        while let Some(chunk) = response.chunk().await? {
            hasher.update(&chunk);
            file.write_all(&chunk)?;
            received += chunk.len() as u64;
            progress.store((received * 1000 / update.size.max(1)).min(1000) as u32, Ordering::Relaxed);
        }
        file.sync_all()?;
        let actual = hex(&hasher.finalize());
        ensure!(actual == expected, "the download is damaged (checksum mismatch); try again");
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    Ok(staged)
}

/// Puts the downloaded build in place of the running executable. Returns the path to start.
pub fn install(staged: &Path) -> Result<PathBuf> {
    let current = std::env::current_exe().context("can't tell where the app is")?;
    let old = old_path(&current);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&current, &old).with_context(|| format!("can't replace {}", current.display()))?;
    if let Err(e) = std::fs::rename(staged, &current) {
        let _ = std::fs::rename(&old, &current);
        let _ = std::fs::remove_file(staged);
        return Err(e).context("couldn't put the new version in place");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o755))?;
    }
    info!(path = %current.display(), "update installed");
    Ok(current)
}

/// Removes what an earlier update left: the replaced executable and any unfinished download.
/// The previous process may still be exiting, so this keeps trying for a few seconds.
pub fn clean_up_previous() {
    let Ok(current) = std::env::current_exe() else { return };
    let mut leftovers = vec![old_path(&current), with_suffix(&current, ".download")];
    leftovers.retain(|p| p.exists() && std::fs::remove_file(p).is_err());
    if leftovers.is_empty() {
        return;
    }
    // Still in use: the previous process is finishing its exit.
    std::thread::spawn(move || {
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            leftovers.retain(|p| p.exists() && std::fs::remove_file(p).is_err());
            if leftovers.is_empty() {
                return;
            }
        }
    });
}

fn staged_path() -> Result<PathBuf> {
    Ok(with_suffix(&std::env::current_exe()?, ".download"))
}

fn old_path(exe: &Path) -> PathBuf {
    with_suffix(exe, ".old")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(suffix);
    path.with_file_name(name)
}

fn client() -> Result<reqwest::Client> {
    // reqwest shares iroh's rustls build, which leaves choosing the crypto provider to the app.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .user_agent(concat!("Connect/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("starting the HTTP client")
}

/// "v1.2.3", "1.2.3" or "1.2.3-beta" → (1, 2, 3).
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let core = text.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// The hash listed for `file` in a `sha256sum` listing.
fn expected_hash(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == file).then(|| hash.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fails with a readable reason if this copy of the app can't update itself in place.
pub fn check_writable() -> Result<()> {
    let probe = with_suffix(&std::env::current_exe()?, ".write-test");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => bail!("the folder Connect is in can't be written to ({e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v0.2.1"), Some((0, 2, 1)));
        assert_eq!(parse_version("1.10.0-beta.2"), Some((1, 10, 0)));
        assert_eq!(parse_version("0.2"), None);
        assert_eq!(parse_version("v0.2.1.4"), None);
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
    }

    #[test]
    fn a_packaged_copy_is_left_to_the_package_manager() {
        assert_eq!(in_system_folder(Path::new("/usr/bin/connect")), cfg!(unix));
        assert!(!in_system_folder(Path::new("/home/me/Downloads/connect-linux-x86_64")));
    }

    #[test]
    fn checksum_listing() {
        let sums = "AB12  connect.exe\ncd34 *connect-linux-x86_64\n";
        assert_eq!(expected_hash(sums, "connect.exe").as_deref(), Some("ab12"));
        assert_eq!(expected_hash(sums, "connect-linux-x86_64").as_deref(), Some("cd34"));
        assert_eq!(expected_hash(sums, "other"), None);
    }

    #[test]
    fn side_files_sit_next_to_the_app() {
        let exe = Path::new("/apps/Connect/connect.exe");
        assert_eq!(old_path(exe), Path::new("/apps/Connect/connect.exe.old"));
        assert_eq!(with_suffix(exe, ".download"), Path::new("/apps/Connect/connect.exe.download"));
    }
}
