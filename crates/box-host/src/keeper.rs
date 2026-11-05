//! What `box-keeper` decides, apart from the processes it runs: which requests on its fifo to
//! take, and how long to wait before starting a voxl that died again.

use std::path::Path;
use std::time::Duration;

use box_desktop::slot_is_allocated;

/// A voxl that lasted this long is not crash-looping, so its next restart starts the delays over.
pub const STABLE: Duration = Duration::from_secs(30);

const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);

/// A request on the keeper's fifo. Anything running as the box user can write to it, which is why
/// it carries a display number and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// The entrypoint finished the desktop: start the daemons.
    Start,
    /// Start the voxl of this display.
    Up(u16),
    /// Stop the voxl of this display.
    Down(u16),
}

impl Request {
    pub fn parse(line: &str) -> Option<Self> {
        let mut words = line.split_whitespace();
        let request = match (words.next()?, words.next()) {
            ("start", None) => Self::Start,
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
        assert_eq!(Request::parse("start\n"), Some(Request::Start));
        assert_eq!(Request::parse("up 2\n"), Some(Request::Up(2)));
        assert_eq!(Request::parse("down 12\n"), Some(Request::Down(12)));
    }

    #[test]
    fn refuses_the_rest() {
        for line in [
            "", "up", "up x", "up 123", "up -1", "up 2 3", "start 1", "down", "rm -rf /",
        ] {
            assert_eq!(Request::parse(line), None, "{line:?}");
        }
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
