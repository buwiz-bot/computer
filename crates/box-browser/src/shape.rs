//! ultra-instinct's answers, read into this crate's own shapes. Nothing of theirs is passed on as
//! it came: every field is picked, every string is clipped, every list is cut, and a field's text
//! is kept only when it's known not to be a password's ([`may_return_value`]).

use serde_json::Value;

use crate::{BrowserError, Rect};

/// Characters kept of a label, a role, a title or an address.
pub const MAX_LABEL_CHARS: usize = 160;
pub const MAX_URL_CHARS: usize = 2048;
/// Characters kept of a field's text.
pub const MAX_VALUE_CHARS: usize = 400;
/// Actions kept per element, and characters per action.
pub const MAX_ACTIONS: usize = 8;
const MAX_ACTION_CHARS: usize = 32;
/// Elements read from one answer, whatever the snapshot keeps.
pub const MAX_READ: usize = 5000;
pub const MAX_TABS: usize = 100;
/// The text rendering of a snapshot.
pub const MAX_RENDER_BYTES: usize = 64 * 1024;

pub fn clip(s: &str, chars: usize) -> String {
    match s.char_indices().nth(chars) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_string(),
    }
}

/// One interactive element as the page has it now.
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    /// ultra-instinct's 1-based index in reading order: what its tools take.
    pub index: usize,
    /// ultra-instinct's stable region id: what a ref is checked against.
    pub region: String,
    pub role: String,
    pub label: String,
    pub actions: Vec<String>,
    pub rect: Rect,
    pub sensitive: bool,
    pub value: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    /// ultra-instinct's snapshot of this state: an index press names it, and is refused
    /// `stale_ref` when the element it showed has moved or changed since.
    pub snapshot: Option<Value>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub elements: Vec<Seen>,
    /// Elements the answer said it had.
    pub total: usize,
}

fn text(v: &Value, key: &str, chars: usize) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(|s| clip(s, chars))
}

fn number(v: &Value, key: &str) -> f64 {
    v.get(key)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .unwrap_or(0.0)
}

/// Whether an element is, or may be, a password field: one ultra-instinct marks so, or whose input
/// type is `password`.
fn is_sensitive(el: &Value) -> bool {
    el.get("sensitive").and_then(Value::as_bool) == Some(true)
        || el
            .get("input_type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("password"))
}

/// Whether a field's text may be returned: only when its input type is known and isn't a
/// password's or a hidden one's, and its role is known. A type that can't be read could be a
/// password, so no type is no text: the same rule box-a11y keeps for AT-SPI.
pub fn may_return_value(role: &str, input_type: Option<&str>, sensitive: bool) -> bool {
    let role = role.trim();
    !sensitive
        && !role.is_empty()
        && !role.eq_ignore_ascii_case("unknown")
        && input_type.map(str::trim).is_some_and(|t| {
            !t.is_empty() && !matches!(t.to_ascii_lowercase().as_str(), "password" | "hidden")
        })
}

/// A `browser_get_state` `format: json` answer.
pub fn page(answer: &Value) -> Result<Page, BrowserError> {
    let elements = answer
        .get("elements")
        .and_then(Value::as_array)
        .ok_or_else(|| BrowserError::Backend("the page's state has no elements".into()))?;
    let mut seen = Vec::with_capacity(elements.len().min(MAX_READ));
    for el in elements.iter().take(MAX_READ) {
        let Some(index) = el.get("index").and_then(Value::as_u64) else {
            continue;
        };
        let Some(region) = el.get("region").and_then(Value::as_str) else {
            continue;
        };
        if index == 0 || region.is_empty() || region.len() > 128 {
            continue;
        }
        let role = text(el, "role", MAX_LABEL_CHARS).unwrap_or_default();
        let sensitive = is_sensitive(el);
        let input_type = el.get("input_type").and_then(Value::as_str);
        let value = if may_return_value(&role, input_type, sensitive) {
            text(el, "value", MAX_VALUE_CHARS)
        } else {
            None
        };
        let rect = el.get("rect").cloned().unwrap_or(Value::Null);
        seen.push(Seen {
            index: usize::try_from(index).unwrap_or(usize::MAX),
            region: region.to_string(),
            role,
            label: text(el, "label", MAX_LABEL_CHARS).unwrap_or_default(),
            actions: el
                .get("actions")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .take(MAX_ACTIONS)
                        .map(|s| clip(s, MAX_ACTION_CHARS))
                        .collect()
                })
                .unwrap_or_default(),
            rect: Rect {
                x: number(&rect, "x"),
                y: number(&rect, "y"),
                width: number(&rect, "w"),
                height: number(&rect, "h"),
            },
            sensitive,
            value,
        });
    }
    Ok(Page {
        snapshot: answer.get("snapshot").filter(|v| !v.is_null()).cloned(),
        url: text(answer, "url", MAX_URL_CHARS),
        title: text(answer, "title", MAX_LABEL_CHARS),
        total: answer
            .get("interactive_elements")
            .and_then(Value::as_u64)
            .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
            .unwrap_or(elements.len()),
        elements: seen,
    })
}

/// The lines a model reads: `@eN role "label"`, a field's value when it may be shown, and
/// `[sensitive]` for a password field. Cut at [`MAX_RENDER_BYTES`].
pub fn render(elements: &[crate::Element]) -> String {
    let mut out = String::new();
    for el in elements {
        let mut line = format!("{} {} {:?}", el.reference, el.role, el.label);
        if el.sensitive {
            line.push_str(" [sensitive]");
        } else if let Some(value) = &el.value {
            line.push_str(&format!(" value={value:?}"));
        }
        line.push('\n');
        if out.len() + line.len() > MAX_RENDER_BYTES {
            out.push_str("…\n");
            break;
        }
        out.push_str(&line);
    }
    out
}

/// Width and height from a PNG's header, or `None` if it isn't one.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    if png.len() < 24 || png[..8] != SIGNATURE || &png[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((width, height))
}

/// A JPEG's width and height, from its first start-of-frame marker (baseline, progressive or any
/// other SOFn); `None` for anything that isn't a JPEG whose frame header can be read.
pub fn jpeg_size(jpeg: &[u8]) -> Option<(u32, u32)> {
    if jpeg.len() < 4 || jpeg[..2] != [0xFF, 0xD8] {
        return None;
    }
    let mut at = 2;
    while at + 4 <= jpeg.len() {
        if jpeg[at] != 0xFF {
            return None;
        }
        let marker = jpeg[at + 1];
        // fill bytes, and markers with no length
        if marker == 0xFF {
            at += 1;
            continue;
        }
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            at += 2;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
        if len < 2 {
            return None;
        }
        // SOF0..SOF15, but not DHT (C4), JPG (C8) or DAC (CC)
        let sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if sof {
            let header = jpeg.get(at + 4..at + 9)?;
            let height = u32::from(u16::from_be_bytes([header[1], header[2]]));
            let width = u32::from(u16::from_be_bytes([header[3], header[4]]));
            return (width > 0 && height > 0).then_some((width, height));
        }
        // the frame header comes before the scan's data
        if marker == 0xDA {
            return None;
        }
        at += 2 + len;
    }
    None
}

/// A list of origin names from an answer's `key`, each either a string or `{origin: …}`.
pub fn origins(answer: &Value, key: &str) -> Vec<String> {
    answer
        .get(key)
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|o| o.as_str().or_else(|| o.get("origin")?.as_str()))
                .take(MAX_TABS)
                .map(|o| clip(o, MAX_URL_CHARS))
                .collect()
        })
        .unwrap_or_default()
}

/// A count from an answer: a number, or the length of a list.
pub fn count(answer: &Value, key: &str) -> u64 {
    match answer.get(key) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::Array(a)) => a.len() as u64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_value_is_returned_only_for_a_known_type_that_is_not_a_password() {
        assert!(may_return_value("textbox", Some("text"), false));
        assert!(may_return_value("textbox", Some("email"), false));
        // A type that couldn't be read could be a password.
        assert!(!may_return_value("textbox", None, false));
        assert!(!may_return_value("textbox", Some(""), false));
        assert!(!may_return_value("textbox", Some("password"), false));
        assert!(!may_return_value("textbox", Some("PASSWORD"), false));
        assert!(!may_return_value("textbox", Some("hidden"), false));
        assert!(!may_return_value("textbox", Some("text"), true));
        assert!(!may_return_value("unknown", Some("text"), false));
        assert!(!may_return_value("", Some("text"), false));
    }

    #[test]
    fn a_password_field_loses_its_text_and_says_so() {
        let answer = json!({
            "url": "https://shop.example/login",
            "title": "Sign in",
            "interactive_elements": 3,
            "elements": [
                {"index": 1, "region": "n1", "role": "textbox", "label": "Email",
                 "input_type": "email", "value": "kid@example.com",
                 "actions": ["type"], "rect": {"x": 1, "y": 2, "w": 3, "h": 4}},
                {"index": 2, "region": "n2", "role": "textbox", "label": "Password",
                 "input_type": "password", "value": "hunter2", "actions": ["type"]},
                {"index": 3, "region": "n3", "role": "textbox", "label": "Note",
                 "value": "no type, no text", "actions": ["type"]},
            ]
        });
        let page = page(&answer).unwrap();
        assert_eq!(page.elements[0].value.as_deref(), Some("kid@example.com"));
        assert!(!page.elements[0].sensitive);
        assert_eq!(page.elements[1].value, None);
        assert!(page.elements[1].sensitive);
        assert_eq!(page.elements[2].value, None);
        assert!(!format!("{page:?}").contains("hunter2"));
        assert_eq!(page.elements[0].rect.height, 4.0);
    }

    #[test]
    fn strings_are_clipped_and_lists_cut() {
        let long = "x".repeat(MAX_LABEL_CHARS * 3);
        let answer = json!({
            "elements": [{"index": 1, "region": "n1", "role": "button", "label": long,
                          "actions": ["a","b","c","d","e","f","g","h","i","j"]}]
        });
        let page = page(&answer).unwrap();
        assert_eq!(page.elements[0].label.chars().count(), MAX_LABEL_CHARS + 1);
        assert_eq!(page.elements[0].actions.len(), MAX_ACTIONS);
        assert_eq!(page.total, 1);
    }

    #[test]
    fn an_answer_with_no_elements_is_the_backend_s_fault() {
        assert!(matches!(page(&json!({})), Err(BrowserError::Backend(_))));
    }

    #[test]
    fn a_png_header_gives_its_size() {
        let mut png = vec![
            0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0, 0, 13,
        ];
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1280u32.to_be_bytes());
        png.extend_from_slice(&800u32.to_be_bytes());
        assert_eq!(png_size(&png), Some((1280, 800)));
        assert_eq!(png_size(b"not a png at all, not at all"), None);
    }
}
