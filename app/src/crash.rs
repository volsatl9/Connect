//! What the app leaves behind when it fails. Built without a console on Windows, it would
//! otherwise vanish without a word.

use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::OnceLock,
};

const LOG: &str = "connect.log";
const PREVIOUS_LOG: &str = "connect.previous.log";

static FOLDER: OnceLock<PathBuf> = OnceLock::new();

/// Starts this run's log in `folder`, keeping the last run's, which saw any crash.
pub fn open_log(folder: &Path) -> Option<File> {
    std::fs::create_dir_all(folder).ok()?;
    let _ = std::fs::rename(folder.join(LOG), folder.join(PREVIOUS_LOG));
    File::create(folder.join(LOG)).ok()
}

/// Writes a crash file to `folder` if the app panics, or on Windows if it faults.
pub fn watch(folder: PathBuf) {
    let _ = FOLDER.set(folder);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        record(&format!("{info}\n\n{backtrace}"));
        previous(info);
    }));
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter;
        SetUnhandledExceptionFilter(Some(fault));
    }
}

fn record(what: &str) {
    let Some(folder) = FOLDER.get() else { return };
    let stamp = connect_perf::session_stamp();
    let version = env!("CARGO_PKG_VERSION");
    let thread = std::thread::current();
    let text = format!(
        "Connect {version} on {} crashed at {stamp}, on thread {}\n\n{what}\n",
        std::env::consts::OS,
        thread.name().unwrap_or("unnamed"),
    );
    let _ = std::fs::create_dir_all(folder);
    let _ = std::fs::write(folder.join(format!("crash-{stamp}.txt")), text);
}

#[cfg(windows)]
unsafe extern "system" fn fault(info: *const windows::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    const CONTINUE_SEARCH: i32 = 0;
    let record = unsafe { info.as_ref().and_then(|info| info.ExceptionRecord.as_ref()) };
    if let Some(fault) = record {
        self::record(&format!("Fault {:#x} at {:?}", fault.ExceptionCode.0, fault.ExceptionAddress));
    }
    CONTINUE_SEARCH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crash_leaves_a_file_saying_what_happened() {
        let dir = std::env::temp_dir().join(format!("connect-crash-file-{}", std::process::id()));
        FOLDER.set(dir.clone()).unwrap();
        record("what went wrong");
        let file = std::fs::read_dir(&dir).unwrap().flatten().next().unwrap();
        assert!(file.file_name().to_string_lossy().starts_with("crash-"));
        let text = std::fs::read_to_string(file.path()).unwrap();
        assert!(text.contains("what went wrong") && text.contains(env!("CARGO_PKG_VERSION")), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_last_runs_log_is_kept() {
        let dir = std::env::temp_dir().join(format!("connect-crash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(LOG), "last run").unwrap();
        assert!(open_log(&dir).is_some());
        assert_eq!(std::fs::read_to_string(dir.join(PREVIOUS_LOG)).unwrap(), "last run");
        assert_eq!(std::fs::read_to_string(dir.join(LOG)).unwrap(), "");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
