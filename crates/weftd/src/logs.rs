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
}
