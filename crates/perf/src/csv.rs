//! Plain CSV: a header row, then one row per record. Numbers only (plus a few fixed words), so
//! nothing needs quoting; empty cells mean "not measured".

use std::{
    fmt::Write as _,
    fs::File,
    io::Write as _,
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::mpsc,
    thread::JoinHandle,
    time::Duration,
};

use crate::TraceRing;

/// A trace record that can be written as a CSV row.
pub trait CsvRecord {
    /// The column names, comma-separated, in row order.
    const HEADER: &'static str;
    /// Appends this record's cells, comma-separated, without the line break.
    fn write_row(&self, out: &mut String);
}

/// Writes `records` to `path` as CSV, replacing the file. Builds the text first, so this is
/// for the end of a session or an explicit export, not the video threads.
pub fn write_csv<T: CsvRecord>(path: &Path, records: &[T]) -> std::io::Result<()> {
    let mut text = String::with_capacity(64 + records.len() * 160);
    text.push_str(T::HEADER);
    text.push('\n');
    for record in records {
        record.write_row(&mut text);
        text.push('\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(text.as_bytes())
}

/// One line of a session's summary file.
pub struct SessionRow(pub &'static str, pub String);

impl CsvRecord for SessionRow {
    const HEADER: &'static str = "key,value";

    fn write_row(&self, out: &mut String) {
        let _ = write!(out, "{},{}", self.0, self.1);
    }
}

/// A CSV file that grows as a session goes on, so a long session keeps every record, not just
/// the ones still in the ring, and a crash loses a few seconds rather than all of it.
pub struct CsvStream<T> {
    path: PathBuf,
    file: Option<File>,
    /// How far into the ring this file has got.
    mark: u64,
    /// Records the ring overwrote before they got here.
    overrun: u64,
    records: PhantomData<T>,
}

impl<T: CsvRecord + Copy> CsvStream<T> {
    pub fn new(path: PathBuf) -> Self {
        Self { path, file: None, mark: 0, overrun: 0, records: PhantomData }
    }

    /// Appends what `ring` got since the last time. For a thread of its own: this does file I/O.
    pub fn drain(&mut self, ring: &TraceRing<T>) -> std::io::Result<()> {
        let (records, mark) = ring.since(self.mark);
        self.overrun += (mark - self.mark).saturating_sub(records.len() as u64);
        self.mark = mark;
        let mut text = String::with_capacity(64 + records.len() * 160);
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            self.file = Some(File::create(&self.path)?);
            text.push_str(T::HEADER);
            text.push('\n');
        }
        for record in &records {
            record.write_row(&mut text);
            text.push('\n');
        }
        self.file.as_mut().expect("opened above").write_all(text.as_bytes())
    }

    pub fn overrun(&self) -> u64 {
        self.overrun
    }
}

/// How often a [`Spool`] writes out what's new. Far less than a ring takes to fill.
const SPOOL_EVERY: Duration = Duration::from_secs(5);

/// Runs `drain` on a thread of its own every few seconds, away from the video threads, and once
/// more with `true` at the end.
pub struct Spool {
    stop: mpsc::Sender<()>,
    thread: JoinHandle<()>,
}

impl Spool {
    pub fn start(mut drain: impl FnMut(bool) + Send + 'static) -> std::io::Result<Self> {
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::Builder::new().name("connect-trace-spool".into()).spawn(move || {
            while stopped.recv_timeout(SPOOL_EVERY) == Err(mpsc::RecvTimeoutError::Timeout) {
                drain(false);
            }
            drain(true);
        })?;
        Ok(Self { stop, thread })
    }

    /// Writes out the rest. Blocks on file I/O.
    pub fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.thread.join();
    }
}

/// Appends `value` and a comma; an empty cell for `None`.
pub fn cell<T: std::fmt::Display>(out: &mut String, value: Option<T>) {
    if let Some(value) = value {
        let _ = write!(out, "{value}");
    }
    out.push(',');
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Row(u32, Option<i64>, &'static str);

    impl CsvRecord for Row {
        const HEADER: &'static str = "seq,value_us,status";
        fn write_row(&self, out: &mut String) {
            cell(out, Some(self.0));
            cell(out, self.1);
            out.push_str(self.2);
        }
    }

    impl Clone for Row {
        fn clone(&self) -> Self {
            *self
        }
    }
    impl Copy for Row {}

    #[test]
    fn a_stream_keeps_more_than_the_ring_does() {
        let dir = std::env::temp_dir().join(format!("connect-perf-stream-{}", std::process::id()));
        let path = dir.join("trace.csv");
        let ring = TraceRing::new(4);
        let mut stream = CsvStream::new(path.clone());
        for seq in 0..10 {
            for offset in 0..3 {
                ring.push(Row(seq * 3 + offset, None, "shown"));
            }
            stream.drain(&ring).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 31, "a header and every record");
        assert_eq!((stream.overrun(), text.lines().nth(30)), (0, Some("29,,shown")));
        // Falling behind says so.
        (0..6).for_each(|seq| ring.push(Row(seq, None, "shown")));
        stream.drain(&ring).unwrap();
        assert_eq!(stream.overrun(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_spool_finishes_what_it_started() {
        let (tx, rx) = mpsc::channel();
        Spool::start(move |last| tx.send(last).unwrap()).unwrap().finish();
        assert_eq!(rx.iter().collect::<Vec<_>>(), [true]);
    }

    #[test]
    fn writes_a_header_and_one_row_per_record() {
        let dir = std::env::temp_dir().join(format!("connect-perf-csv-{}", std::process::id()));
        let path = dir.join("nested").join("trace.csv");
        write_csv(&path, &[Row(1, Some(-5), "shown"), Row(2, None, "superseded")]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "seq,value_us,status\n1,-5,shown\n2,,superseded\n");
        // Every row has as many cells as the header.
        let columns = text.lines().next().unwrap().split(',').count();
        assert!(text.lines().all(|line| line.split(',').count() == columns));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
