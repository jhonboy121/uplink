//! The log on disk: a fixed set of files rolled by size, and the archive of them that a tester
//! sends back.
//!
//! Rolling by size rather than by run matters because a run has no bounded length — a call that
//! goes wrong on the twentieth launch of the day used to take the evidence of the first nineteen
//! with it, and a phone left running for a week wrote one unbounded file.
//!
//! [`Rolling`] is a synchronous [`Write`], because that is what a `tracing` subscriber takes.
//! Nothing calls it on an app thread: `tracing_appender::non_blocking` hands the writes to a
//! worker, so a log line costs a channel send.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;

use crate::Error;

/// Bytes a log file reaches before the set rolls.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Files kept, the live one included.
pub const KEEP: usize = 10;

/// Appends to `<stem>.log`, rolling it to `<stem>.1.log` … `<stem>.{KEEP-1}.log` by size and
/// dropping the oldest. Reopening appends rather than rolling, so restarting the app costs
/// nothing.
pub struct Rolling {
    dir: PathBuf,
    stem: String,
    max_bytes: u64,
    keep: usize,
    file: File,
    written: u64,
    /// Whether the last byte written was a newline. A formatted event reaches a writer in
    /// several calls, so this is the only way to roll between lines rather than through one.
    at_line_start: bool,
}

impl Rolling {
    pub fn new(dir: &Path, stem: &str, max_bytes: u64, keep: usize) -> Result<Self, Error> {
        let mut rolling = Self {
            dir: dir.to_path_buf(),
            stem: stem.to_owned(),
            max_bytes,
            keep: keep.max(1),
            file: open(&path_for(dir, stem, 0))?,
            written: 0,
            at_line_start: true,
        };
        rolling.written = rolling.file.metadata()?.len();
        Ok(rolling)
    }

    /// Shifts every kept file one place older and starts a new live file. Best effort: if the
    /// shuffle fails the old file stays open, because losing the log is worse than a long one.
    fn roll(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let live = path_for(&self.dir, &self.stem, 0);
        // Nothing to keep: the live file is the whole set, so it starts again.
        if self.keep == 1 {
            self.file = OpenOptions::new().create(true).write(true).truncate(true).open(&live)?;
            self.written = 0;
            return Ok(());
        }
        let oldest = path_for(&self.dir, &self.stem, self.keep - 1);
        if oldest.exists() {
            std::fs::remove_file(&oldest)?;
        }
        for index in (1..self.keep - 1).rev() {
            let from = path_for(&self.dir, &self.stem, index);
            if from.exists() {
                std::fs::rename(&from, path_for(&self.dir, &self.stem, index + 1))?;
            }
        }
        std::fs::rename(&live, path_for(&self.dir, &self.stem, 1))?;
        self.file = open(&live)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for Rolling {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Rolled after the line that crosses the mark, never through one, so every file is whole
        // lines and a line longer than the limit still lands somewhere in one piece. A file may
        // therefore end up one line over `max_bytes`.
        if self.at_line_start && self.written >= self.max_bytes {
            self.roll()?;
        }
        let written = self.file.write(buf)?;
        if let Some(last) = buf.get(..written).and_then(<[u8]>::last) {
            self.at_line_start = *last == b'\n';
        }
        self.written = self.written.saturating_add(u64::try_from(written).unwrap_or_default());
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// `<stem>.log` for the live file, `<stem>.{index}.log` for the rest.
fn path_for(dir: &Path, stem: &str, index: usize) -> PathBuf {
    match index {
        0 => dir.join(format!("{stem}.log")),
        index => dir.join(format!("{stem}.{index}.log")),
    }
}

/// Packs the kept files, oldest first, into a gzipped tar at `out`.
///
/// Reading and writing go through `tokio::fs`, which is a thread pool rather than true async —
/// Android has no completion-based file API — but it keeps the work off the UI thread.
/// Compression is CPU, so the task yields between files rather than holding an executor thread
/// for the length of the whole archive.
pub async fn archive(dir: &Path, stem: &str, keep: usize, out: &Path) -> Result<(), Error> {
    let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for index in (0..keep).rev() {
        let path = path_for(dir, stem, index);
        let Ok(data) = tokio::fs::read(&path).await else { continue };
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(data.len()).unwrap_or_default());
        header.set_mode(FILE_MODE);
        header.set_cksum();
        tar.append_data(&mut header, name, data.as_slice())?;
        tokio::task::yield_now().await;
    }
    let bytes = tar.into_inner()?.finish()?;
    if let Some(parent) = out.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    Ok(tokio::fs::write(out, bytes).await?)
}

/// Owner read/write, everyone else read: the archive leaves the app, so it is not private.
const FILE_MODE: u32 = 0o644;

#[cfg(test)]
mod tests {
    use super::*;

    /// Rolls once the live file is full, and never grows the set past `keep`.
    #[test]
    fn the_set_rolls_and_stays_the_same_size() -> Result<(), Error> {
        let dir = tempfile::tempdir()?;
        let keep = 4;
        let mut log = Rolling::new(dir.path(), "uplink", 32, keep)?;
        for line in 0..40 {
            writeln!(log, "line {line} ........................")?;
        }
        log.flush()?;
        let files = std::fs::read_dir(dir.path())?.count();
        assert_eq!(files, keep, "kept {files} files, wanted {keep}");
        // The newest line is in the live file, not in an archived one.
        let live = std::fs::read_to_string(path_for(dir.path(), "uplink", 0))?;
        assert!(live.contains("line 39"), "the live file is not the newest: {live}");
        Ok(())
    }

    /// Reopening continues the live file. A crash loop would otherwise roll the evidence away.
    #[test]
    fn reopening_appends() -> Result<(), Error> {
        let dir = tempfile::tempdir()?;
        for run in 0..3 {
            let mut log = Rolling::new(dir.path(), "uplink", MAX_BYTES, KEEP)?;
            writeln!(log, "run {run}")?;
            log.flush()?;
        }
        let live = std::fs::read_to_string(path_for(dir.path(), "uplink", 0))?;
        assert_eq!(live.lines().count(), 3, "{live}");
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }

    /// A line longer than the limit still gets written, to its own file.
    #[test]
    fn an_oversized_line_is_not_lost() -> Result<(), Error> {
        let dir = tempfile::tempdir()?;
        let mut log = Rolling::new(dir.path(), "uplink", 8, KEEP)?;
        let long = "x".repeat(64);
        writeln!(log, "{long}")?;
        log.flush()?;
        assert!(std::fs::read_to_string(path_for(dir.path(), "uplink", 0))?.contains(&long));
        Ok(())
    }

    /// What the tester sends: every kept file, in one archive, oldest first.
    #[tokio::test]
    async fn the_archive_holds_every_kept_file() -> Result<(), Error> {
        let dir = tempfile::tempdir()?;
        let mut log = Rolling::new(dir.path(), "uplink", 64, 3)?;
        for line in 0..20 {
            writeln!(log, "line {line} ....................")?;
        }
        log.flush()?;
        let out = dir.path().join("share/uplink-logs.tar.gz");
        archive(dir.path(), "uplink", 3, &out).await?;

        let packed = std::fs::File::open(&out)?;
        let mut names = Vec::new();
        for entry in tar::Archive::new(flate2::read::GzDecoder::new(packed)).entries()? {
            names.push(entry?.path()?.to_string_lossy().into_owned());
        }
        assert_eq!(names, ["uplink.2.log", "uplink.1.log", "uplink.log"]);
        Ok(())
    }
}
