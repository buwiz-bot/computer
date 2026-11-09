//! The page in front of Chromium, as the box's own extension reports it.
//!
//! No Chromium in the box has a DevTools port, so that nothing else in the box can drive it or
//! read its cookies. To know which page is in front on every screen, the image loads a small
//! extension (`docker/active-tab`) with only the `tabs` permission. It sends the front tab's URL
//! over native messaging to `box-active-tab`, which keeps the latest in [`file_for`]: that
//! screen's `/run/box/x/<slot>/run/active-tab.json`, in a run folder its desktop user owns, the
//! services may read and a command can't reach. `GET /v1/chrome/active-tab` reads it back with
//! [`read_screen`], which follows no link, waits on no fifo and, when `BOX_RUN_AS=agents`, takes
//! the file only if that screen's desktop user owns it.
//!
//! The server asks before it types a saved login, and types nothing when the page in front is
//! not the login's own site. The file is written by the screen's desktop user, so anything
//! running as that user (its Chromium included) could write it too. It answers "which page is
//! in front", not "prove no one lied".

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub use box_desktop::active_tab_file as file_for;

/// Native messaging caps a message to the host at 64 MiB; a URL is far smaller, and anything
/// past this is not the extension speaking.
const MAX_MESSAGE: u32 = 64 * 1024;

/// The page in front, or none known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveTab {
    /// The front tab's URL; `None` when nothing has reported (no Chromium, or one started
    /// without the extension).
    pub url: Option<String>,
    /// When the extension last reported, in ms since the epoch.
    #[serde(rename = "atMs", skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
    /// What kind of box has focus on that page, as its page said (`docker/active-tab/focus.js`):
    /// `password`, `text`, `other` or `none`; `None` while the page has not said. The server
    /// types a password only into a `password` box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
    /// How many reports this browser has made: a reader that clicked a field waits for it to
    /// move before it trusts `focus`, so an older report is not taken for the click's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

/// One native-messaging message: a 4-byte length in native byte order, then that much JSON.
/// `None` at a clean end of input, which is Chromium closing the port.
pub fn read_message(input: &mut impl Read) -> io::Result<Option<serde_json::Value>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let len = u32::from_ne_bytes(len);
    if len > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too long",
        ));
    }
    let mut body = vec![0u8; len as usize];
    input.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Keep `url` as the page in front, with the kind of box focused on it and the report's number:
/// a new file renamed over the old, so a reader never sees half of one.
pub fn record(path: &Path, url: &str, focus: Option<&str>, seq: u64) -> io::Result<()> {
    let at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // Only the words the extension uses: anything else is not the extension speaking.
    let focus = focus.filter(|f| matches!(*f, "password" | "text" | "other" | "none"));
    let tab = ActiveTab {
        url: Some(url.to_string()),
        at_ms: Some(at_ms),
        focus: focus.map(str::to_string),
        seq: Some(seq),
    };
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec(&tab)?)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)
}

/// What the file says, or nothing known when it is missing or unreadable.
pub fn read(path: &Path) -> ActiveTab {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(ActiveTab {
            url: None,
            at_ms: None,
            focus: None,
            seq: None,
        })
}

/// What a screen's report says, read so that nothing its folder holds can mislead this:
/// [`box_desktop::read_report`].
pub fn read_screen(display: Option<&str>) -> ActiveTab {
    box_desktop::read_report(display)
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(ActiveTab {
            url: None,
            at_ms: None,
            focus: None,
            seq: None,
        })
}

/// The host's whole life: record every report until Chromium closes the port, then forget,
/// so a browser that is gone is never taken for one showing its last page.
pub fn serve(input: &mut impl Read, path: &Path) -> io::Result<()> {
    let result = (|| {
        let mut seq = 0;
        while let Some(message) = read_message(input)? {
            if let Some(url) = message.get("url").and_then(serde_json::Value::as_str) {
                seq += 1;
                let focus = message.get("focus").and_then(serde_json::Value::as_str);
                record(path, url, focus, seq)?;
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(json: &str) -> Vec<u8> {
        let mut bytes = (json.len() as u32).to_ne_bytes().to_vec();
        bytes.extend_from_slice(json.as_bytes());
        bytes
    }

    #[test]
    fn the_last_report_is_what_is_in_front_until_the_port_closes() {
        let dir = std::env::temp_dir().join(format!("active-tab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tab.json");
        let mut input = framed(r#"{"url":"https://www.youtube.com/"}"#);
        input.extend(framed(r#"{"url":"https://www.facebook.com/login"}"#));

        // Read as it arrives: after both reports, the second is in front.
        let mut reader = &input[..];
        while let Some(message) = read_message(&mut reader).unwrap() {
            record(&path, message["url"].as_str().unwrap(), None, 1).unwrap();
        }
        assert_eq!(
            read(&path).url.as_deref(),
            Some("https://www.facebook.com/login")
        );

        // The whole life: once Chromium closes the port nothing is known.
        serve(&mut &input[..], &path).unwrap();
        assert_eq!(read(&path).url, None);
    }

    /// The focused box's kind comes through with each report, numbered, and only in the
    /// extension's own words.
    #[test]
    fn each_report_says_what_kind_of_box_has_focus() {
        let dir = std::env::temp_dir().join(format!("active-tab-focus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tab.json");
        let fb = "https://www.facebook.com/login";
        let mut input = framed(&format!(r#"{{"url":"{fb}","focus":"text"}}"#));
        input.extend(framed(&format!(r#"{{"url":"{fb}","focus":"password"}}"#)));
        input.extend(framed(&format!(r#"{{"url":"{fb}","focus":"<script>"}}"#)));

        let mut reader = &input[..];
        let mut seq = 0;
        let mut seen = Vec::new();
        while let Some(message) = read_message(&mut reader).unwrap() {
            seq += 1;
            let focus = message["focus"].as_str();
            record(&path, message["url"].as_str().unwrap(), focus, seq).unwrap();
            let now = read(&path);
            seen.push((now.focus, now.seq));
        }
        assert_eq!(
            seen,
            [
                (Some("text".to_string()), Some(1)),
                (Some("password".to_string()), Some(2)),
                (None, Some(3)),
            ]
        );
    }

    /// Each screen's Chromium reports to its own file.
    #[test]
    fn each_screen_has_its_own_report() {
        let main = Path::new("/run/box/x/1/run/active-tab.json");
        assert_eq!(file_for(None), main);
        assert_eq!(file_for(Some(":1")), main);
        assert_eq!(file_for(Some(":1.0")), main);
        assert_eq!(
            file_for(Some(":3")),
            Path::new("/run/box/x/3/run/active-tab.json")
        );
        assert_eq!(file_for(Some("../../etc")), main);
        assert_eq!(file_for(Some(":3/../x")), main);
    }

    #[test]
    fn a_message_longer_than_any_url_is_refused() {
        let input = (MAX_MESSAGE + 1).to_ne_bytes();
        assert!(read_message(&mut &input[..]).is_err());
    }
}
