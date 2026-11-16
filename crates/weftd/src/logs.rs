use std::collections::VecDeque;
use std::io;
use std::sync::{Mutex, OnceLock};

use tracing_subscriber::fmt::MakeWriter;

const CAPACITY: usize = 500;
const MAX_LINE: usize = 1000;

static RECENT: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

fn buffer() -> &'static Mutex<VecDeque<String>> {
    RECENT.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

/// The most recent log lines, oldest first.
pub fn recent() -> Vec<String> {
    buffer().lock().map(|lines| lines.iter().cloned().collect()).unwrap_or_default()
}

/// Keeps formatted log lines in memory for diagnostics.
#[derive(Clone, Copy)]
pub struct Recent;

pub struct Line(Vec<u8>);

impl<'a> MakeWriter<'a> for Recent {
    type Writer = Line;

    fn make_writer(&'a self) -> Line {
        Line(Vec::new())
    }
}

impl io::Write for Line {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.0);
        let Ok(mut lines) = buffer().lock() else { return };
        for line in text.lines().filter(|line| !line.is_empty()) {
            if lines.len() == CAPACITY {
                lines.pop_front();
            }
            lines.push_back(line.chars().take(MAX_LINE).collect());
        }
    }
}

/// Past this size a log file starts over, keeping the previous one as `.1` when it can.
const MAX_LOG: u64 = 10 << 20;

/// A log file that does not grow without end.
#[derive(Clone)]
pub struct Capped(std::sync::Arc<Mutex<CappedFile>>);

struct CappedFile {
    file: std::fs::File,
    /// None for an inherited file, such as stderr that launchd opened.
    path: Option<std::path::PathBuf>,
    size: u64,
}

impl Capped {
    pub fn open(path: std::path::PathBuf) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata()?.len();
        Ok(Self::new(CappedFile { file, path: Some(path), size }))
    }

    /// Standard error, when it was pointed at a file.
    #[cfg(unix)]
    pub fn stderr() -> Option<Self> {
        use std::os::fd::AsFd;
        let file = std::fs::File::from(io::stderr().as_fd().try_clone_to_owned().ok()?);
        let meta = file.metadata().ok().filter(std::fs::Metadata::is_file)?;
        Some(Self::new(CappedFile { file, path: None, size: meta.len() }))
    }

    fn new(file: CappedFile) -> Self {
        Self(std::sync::Arc::new(Mutex::new(file)))
    }
}

impl CappedFile {
    fn rotate(&mut self) -> io::Result<()> {
        match &self.path {
            Some(path) => {
                let mut old = path.clone().into_os_string();
                old.push(".1");
                std::fs::rename(path, old)?;
                self.file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
            }
            // Writes in append mode go to the new end after truncation.
            None => self.file.set_len(0)?,
        }
        self.size = 0;
        Ok(())
    }
}

impl io::Write for Capped {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut log = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if log.size >= MAX_LOG {
            let _ = log.rotate();
        }
        log.file.write_all(data)?;
        log.size += data.len() as u64;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).file.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn keeps_recent_lines() {
        for n in 0..CAPACITY + 3 {
            writeln!(Recent.make_writer(), "line {n}").unwrap();
        }
        let lines = recent();
        assert_eq!(lines.len(), CAPACITY);
        assert_eq!(lines.last().unwrap(), &format!("line {}", CAPACITY + 2));
    }

    #[test]
    fn capped_log_starts_over() {
        let path = std::env::temp_dir().join(format!("weftd-log-{}.log", std::process::id()));
        let mut log = Capped::open(path.clone()).unwrap();
        log.0.lock().unwrap().size = MAX_LOG;
        log.write_all(b"fresh\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
        let mut old = path.clone().into_os_string();
        old.push(".1");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(old);
    }
}
