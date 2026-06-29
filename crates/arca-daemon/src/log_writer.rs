//! Reopenable log writer.
//!
//! `tracing-appender::non_blocking` takes ownership of a `Write` and the worker
//! thread holds that handle indefinitely — meaning newsyslog's
//! rename-then-create cycle leaves the daemon writing to the rotated file
//! (whose inode is unchanged after rename), and the freshly-created
//! `arca.log` stays empty. To make newsyslog work we wrap the underlying
//! `File` in an `Arc<Mutex<File>>`; on SIGUSR1 the signal handler calls
//! `LogReopener::reopen`, which `O_APPEND|O_CREAT`-opens the path again and
//! atomically replaces the inner File. The non-blocking writer continues to
//! call `Write` on the wrapper — all subsequent writes land in the new file.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct ReopenWriter {
    inner: Arc<Mutex<File>>,
}

impl ReopenWriter {
    pub fn open(path: &Path) -> io::Result<(Self, LogReopener)> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let inner = Arc::new(Mutex::new(file));
        let reopener = LogReopener {
            inner: Arc::clone(&inner),
            path: path.to_owned(),
        };
        Ok((Self { inner }, reopener))
    }
}

impl Write for ReopenWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("log writer mutex poisoned"))?;
        g.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("log writer mutex poisoned"))?;
        g.flush()
    }
}

#[derive(Clone)]
pub struct LogReopener {
    inner: Arc<Mutex<File>>,
    path: PathBuf,
}

impl LogReopener {
    pub fn reopen(&self) -> io::Result<()> {
        let new = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut g = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("log writer mutex poisoned"))?;
        *g = new;
        Ok(())
    }
}
