//! Where the TUI's logs go: stderr would scribble over the screen, so every line goes to a file
//! and to the log pane. The file is `$UPLINK_CLI_LOG` (`just cli` makes it target/cli.log), or
//! else `uplink.log` in the data dir, as the app keeps `files/uplink.log` on a phone.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing_subscriber::fmt::MakeWriter;

pub const FILE: &str = "uplink.log";
pub const PATH_VAR: &str = "UPLINK_CLI_LOG";

#[derive(Clone)]
pub struct Sink {
    file: Arc<Mutex<File>>,
    lines: mpsc::UnboundedSender<String>,
}

impl Sink {
    /// The sink, and the log pane's end of it.
    pub fn open(dir: &Path) -> Result<(Self, mpsc::UnboundedReceiver<String>)> {
        let path = std::env::var_os(PATH_VAR).map_or_else(|| dir.join(FILE), PathBuf::from);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let (lines, pane) = mpsc::unbounded_channel();
        Ok((Self { file: Arc::new(Mutex::new(file)), lines }, pane))
    }
}

/// One event's text, written out whole when the formatter is done with it.
pub struct Line {
    text: Vec<u8>,
    sink: Sink,
}

impl Write for Line {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.text.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        if let Ok(mut file) = self.sink.file.lock() {
            // Nowhere left to say a log write failed.
            file.write_all(&self.text).ok();
        }
        let text = String::from_utf8_lossy(&self.text);
        // The pane is gone once the TUI has stopped; the file still has it.
        self.sink.lines.send(text.trim_end().to_owned()).ok();
    }
}

impl<'a> MakeWriter<'a> for Sink {
    type Writer = Line;

    fn make_writer(&'a self) -> Self::Writer {
        Line { text: Vec::new(), sink: self.clone() }
    }
}
