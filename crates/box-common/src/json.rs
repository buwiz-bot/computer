//! Measuring JSON without writing it out.

use std::io;

use serde::Serialize;

/// The length of `value` as compact JSON (what `serde_json::to_vec` would give), or `None` once it
/// is past `cap` bytes, or when it can't be written as JSON. Nothing is allocated, and writing stops
/// at the first chunk past the cap.
pub fn len_within<T: Serialize + ?Sized>(value: &T, cap: usize) -> Option<usize> {
    let mut count = Count { bytes: 0, cap };
    serde_json::to_writer(&mut count, value).ok()?;
    Some(count.bytes)
}

/// Counts what is written, and refuses once past `cap`.
struct Count {
    bytes: usize,
    cap: usize,
}

impl io::Write for Count {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buf.len());
        if self.bytes > self.cap {
            return Err(io::Error::other("past the cap"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_count_is_what_to_vec_writes() {
        let value = json!({
            "tabs": [{"url": "https://example.com/?q=\"x\"", "title": "Ünïcode \u{1f600}"}, null],
            "n": [1, -2.5, 1e300, true],
            "nested": {"a": {"b": {"c": ["\n\t\\", {}]}}}
        });
        let len = serde_json::to_vec(&value).unwrap().len();
        assert_eq!(len_within(&value, usize::MAX), Some(len));
        assert_eq!(len_within(&value, len), Some(len));
        assert_eq!(len_within(&value, len - 1), None);
    }

    #[test]
    fn past_the_cap_is_refused() {
        let value = json!({"text": "x".repeat(10_000)});
        assert_eq!(len_within(&value, 4096), None);
        assert_eq!(len_within(&value, 0), None);
    }
}
