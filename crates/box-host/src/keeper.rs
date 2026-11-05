//! What `box-keeper` decides, apart from the processes it runs: which requests on its fifo to
//! take, and how long to wait before starting a voxl that died again.

use std::io::{self, BufRead, Read};
use std::path::Path;
use std::time::Duration;

use box_desktop::slot_is_allocated;

/// A voxl that lasted this long is not crash-looping, so its next restart starts the delays over.
pub const STABLE: Duration = Duration::from_secs(30);

const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);

/// Longer than any request. A longer line is dropped whole.
const MAX_LINE: u64 = 64;

/// A request on the keeper's fifo. Anything running as the box user can write to it, which is why
/// it carries a display number and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// Start the voxl of this display.
    Up(u16),
    /// Stop the voxl of this display.
    Down(u16),
}

impl Request {
    pub fn parse(line: &str) -> Option<Self> {
        let mut words = line.split_whitespace();
        let request = match (words.next()?, words.next()) {
            ("up", Some(slot)) => Self::Up(parse_slot(slot)?),
            ("down", Some(slot)) => Self::Down(parse_slot(slot)?),
            _ => return None,
        };
        words.next().is_none().then_some(request)
    }
}

fn parse_slot(word: &str) -> Option<u16> {
    (word.len() <= 2 && word.bytes().all(|b| b.is_ascii_digit()))
        .then(|| word.parse().ok())
        .flatten()
}

/// The next line of the fifo, or `None` at its end. A line over [`MAX_LINE`] comes back empty, and
/// all of it is consumed, so its tail cannot be read as a request of its own.
pub fn next_line(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    if reader
        .by_ref()
        .take(MAX_LINE)
        .read_until(b'\n', &mut line)?
        == 0
    {
        return Ok(None);
    }
    if line.ends_with(b"\n") {
        return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
    }
    if line.len() as u64 == MAX_LINE {
        loop {
            line.clear();
            if reader
                .by_ref()
                .take(MAX_LINE)
                .read_until(b'\n', &mut line)?
                == 0
                || line.ends_with(b"\n")
            {
                break;
            }
        }
    }
    Ok(Some(String::new()))
}

/// Only the box's own display and a slot `box-screen` allocated get a viewer: without this, a
/// process could fill the box with voxls for displays that do not exist.
pub fn may_have_viewer(slot: u16, main: u16, slots_dir: &Path) -> bool {
    slot == main || slot_is_allocated(slots_dir, slot)
}

/// 1 s after the first failure, doubling to 30 s.
pub fn restart_delay(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.min(5)).min(MAX_RESTART_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_the_box_sends() {
        assert_eq!(Request::parse("up 2\n"), Some(Request::Up(2)));
        assert_eq!(Request::parse("down 12\n"), Some(Request::Down(12)));
    }

    #[test]
    fn refuses_the_rest() {
        for line in [
            "", "up", "up x", "up 123", "up -1", "up 2 3", "start", "down", "rm -rf /",
        ] {
            assert_eq!(Request::parse(line), None, "{line:?}");
        }
    }

    #[test]
    fn a_long_line_is_dropped_whole() {
        let long = format!("{}up 2\nup 3\n", "x".repeat(200));
        let mut reader = io::Cursor::new(long);
        assert_eq!(next_line(&mut reader).unwrap().as_deref(), Some(""));
        assert_eq!(next_line(&mut reader).unwrap().as_deref(), Some("up 3\n"));
        assert_eq!(next_line(&mut reader).unwrap(), None);
    }

    #[test]
    fn a_line_of_exactly_the_limit_is_also_dropped() {
        let long = format!("{}\nup 3\n", "x".repeat(MAX_LINE as usize));
        let mut reader = io::Cursor::new(long);
        assert_eq!(next_line(&mut reader).unwrap().as_deref(), Some(""));
        assert_eq!(next_line(&mut reader).unwrap().as_deref(), Some("up 3\n"));
    }

    #[test]
    fn a_viewer_needs_the_main_display_or_an_allocated_slot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("amy"), "3\n").unwrap();
        assert!(may_have_viewer(1, 1, dir.path()));
        assert!(may_have_viewer(3, 1, dir.path()));
        assert!(!may_have_viewer(4, 1, dir.path()));
    }

    #[test]
    fn restarts_back_off_up_to_a_limit() {
        let secs: Vec<_> = (0..8).map(|n| restart_delay(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
    }
}
