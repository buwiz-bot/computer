//! ultra-instinct's answers, read into this crate's own shapes. Nothing of theirs is passed on as
//! it came: every field is picked, every string is clipped, every list is cut, and a field's text
//! is kept only when it was asked for and it's known not to be a password's ([`may_return_value`],
//! [`may_return_selected`]).

use serde_json::Value;

use crate::{BrowserError, Rect};

/// Characters of a label, a role or a title an answer shows.
pub const MAX_LABEL_CHARS: usize = 160;
/// Characters of a label kept to check and confirm by: the guard compares the whole label, so one
/// clipped to what an answer shows could never be confirmed. Past this, a label is clipped too.
pub const MAX_KEPT_LABEL_CHARS: usize = 4096;
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
    /// Whole, up to [`MAX_KEPT_LABEL_CHARS`]: clip it to [`MAX_LABEL_CHARS`] to show it.
    pub label: String,
    pub actions: Vec<String>,
    pub rect: Rect,
    pub sensitive: bool,
    /// What the element holds, only when it was asked for and may be shown: [`page`].
    pub value: Option<String>,
    pub value_truncated: bool,
    pub checked: Option<bool>,
    pub expanded: Option<bool>,
    /// `aria-selected`: ultra-instinct's `selected_item`.
    pub selected: Option<bool>,
    /// `aria-current` (`page`, `step`, …): ultra-instinct's `aria_current`, clipped as a label is.
    pub current: Option<String>,
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
/// password, so no type is no text: the same rule box-a11y keeps for AT-SPI. A checkbox's or a
/// radio's `value` is its form value, not anything typed: its `checked` says what it holds.
pub fn may_return_value(role: &str, input_type: Option<&str>, sensitive: bool) -> bool {
    let role = role.trim();
    !sensitive
        && !role.is_empty()
        && !role.eq_ignore_ascii_case("unknown")
        && input_type.map(str::trim).is_some_and(|t| {
            !t.is_empty()
                && !matches!(
                    t.to_ascii_lowercase().as_str(),
                    "password" | "hidden" | "checkbox" | "radio"
                )
        })
}

/// The roles ultra-instinct gives a choice among a page's own options, at its pin: a native
/// `<select>` is `generic` from the page (which offers `select` only for a SELECT tag), and an
/// ARIA listbox is `listbox`. Not `combobox`, though Chromium's accessibility tree calls a native
/// select one: an ARIA combobox may be a box a person types in, with no input type to say so,
/// and its text is no option of the page's. A text field is `text_field`, so none of these.
const SELECT_ROLES: [&str; 2] = ["generic", "listbox"];

/// Whether a select's chosen option may be returned. ultra-instinct reads an input type only for
/// an `<input>`, so a `<select>` has none and [`may_return_value`] would never let it through.
/// Of the elements with no input type, ultra-instinct reads a value only for a `<textarea>` (no
/// `select` action) and a `<select>` (its chosen option, and it offers `select`): so an element
/// with no input type, a select's role ([`SELECT_ROLES`]) and the `select` action is a choice
/// among the page's own options. Anything else with no input type shows no text: a text widget
/// that also offers `select`, and a combobox, which may hold what a person typed. A sensitive
/// one still shows nothing.
pub fn may_return_selected(
    role: &str,
    input_type: Option<&str>,
    actions: &[&str],
    sensitive: bool,
) -> bool {
    let role = role.trim();
    !sensitive
        && SELECT_ROLES.iter().any(|r| role.eq_ignore_ascii_case(r))
        && input_type.is_none()
        && actions.iter().any(|a| a.eq_ignore_ascii_case("select"))
}

/// A `browser_get_state` `format: json` answer. Only with `values` are `value`, `checked`,
/// `expanded`, `selected_item` and `aria_current` read, and never for a sensitive element,
/// whatever the answer holds.
pub fn page(answer: &Value, values: bool) -> Result<Page, BrowserError> {
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
        let actions: Vec<&str> = el
            .get("actions")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let shown = values
            && (may_return_value(&role, input_type, sensitive)
                || may_return_selected(&role, input_type, &actions, sensitive));
        let raw = el.get("value").and_then(Value::as_str).filter(|_| shown);
        let value = raw.map(|v| clip(v, MAX_VALUE_CHARS));
        let value_truncated = raw.is_some_and(|v| {
            v.chars().nth(MAX_VALUE_CHARS).is_some()
                || el.get("value_truncated").and_then(Value::as_bool) == Some(true)
        });
        let state = |key: &str| {
            el.get(key)
                .and_then(Value::as_bool)
                .filter(|_| values && !sensitive)
        };
        let rect = el.get("rect").cloned().unwrap_or(Value::Null);
        seen.push(Seen {
            index: usize::try_from(index).unwrap_or(usize::MAX),
            region: region.to_string(),
            role,
            label: text(el, "label", MAX_KEPT_LABEL_CHARS).unwrap_or_default(),
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
            value_truncated,
            checked: state("checked"),
            expanded: state("expanded"),
            selected: state("selected_item"),
            current: text(el, "aria_current", MAX_LABEL_CHARS).filter(|_| values && !sensitive),
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

/// The lines a model reads: `@eN role "label"`, `[sensitive]` for a password field, and, when
/// they were asked for and may be shown, a field's ` value="…"`, ` checked`/` unchecked`,
/// ` expanded`/` collapsed`, ` selected` (a selected tab, option or row) and ` current="page"`.
/// Cut at [`MAX_RENDER_BYTES`].
pub fn render(elements: &[crate::Element]) -> String {
    let mut out = String::new();
    for el in elements {
        let mut line = format!("{} {} {:?}", el.reference, el.role, el.label);
        if el.sensitive {
            line.push_str(" [sensitive]");
        } else {
            if let Some(value) = &el.value {
                line.push_str(&format!(" value={value:?}"));
            }
            match el.checked {
                Some(true) => line.push_str(" checked"),
                Some(false) => line.push_str(" unchecked"),
                None => {}
            }
            match el.expanded {
                Some(true) => line.push_str(" expanded"),
                Some(false) => line.push_str(" collapsed"),
                None => {}
            }
            if el.selected == Some(true) {
                line.push_str(" selected");
            }
            if let Some(current) = &el.current {
                line.push_str(&format!(" current={current:?}"));
            }
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
        // a checkbox's or a radio's value is its form value: its `checked` comes instead
        assert!(!may_return_value("checkbox", Some("checkbox"), false));
        assert!(!may_return_value("checkbox", Some("Radio"), false));
    }

    #[test]
    fn a_select_s_option_is_returned_for_an_untyped_element_that_offers_select() {
        assert!(may_return_selected(
            "generic",
            None,
            &["focus", "click", "select"],
            false
        ));
        assert!(!may_return_selected(
            "generic",
            None,
            &["focus", "click", "select"],
            true
        ));
        // a combobox may be a box a person types in, with no input type to say so: no text,
        // though Chromium's accessibility tree calls a native select one too
        assert!(!may_return_selected(
            "combobox",
            None,
            &["focus", "click", "select", "type"],
            false
        ));
        // a textarea: no input type, and nothing to select
        assert!(!may_return_selected(
            "text_field",
            None,
            &["click", "focus", "type"],
            false
        ));
        assert!(!may_return_selected(
            "generic",
            Some("text"),
            &["select"],
            false
        ));
        assert!(!may_return_selected("generic", None, &["click"], false));
        assert!(!may_return_selected("", None, &["select"], false));
        // an ARIA listbox; and any select, sensitive, shows nothing
        assert!(may_return_selected(
            "listbox",
            None,
            &["focus", "click", "select"],
            false
        ));
        for role in ["generic", "combobox", "listbox"] {
            assert!(
                !may_return_selected(role, None, &["select"], true),
                "{role}"
            );
        }
        // a text widget with no input type that also offers select: no type, no text
        assert!(!may_return_selected(
            "text_field",
            None,
            &["click", "focus", "type", "select"],
            false
        ));
        assert!(!may_return_selected("unknown", None, &["select"], false));
    }

    #[test]
    fn states_come_only_when_asked_and_never_from_a_sensitive_element() {
        let long = "y".repeat(MAX_VALUE_CHARS + 5);
        let answer = json!({
            "elements": [
                {"index": 1, "region": "n1", "role": "checkbox", "label": "Gift wrap",
                 "input_type": "checkbox", "checked": true, "actions": ["toggle"]},
                {"index": 2, "region": "n2", "role": "button", "label": "Menu",
                 "expanded": false, "actions": ["click"]},
                {"index": 3, "region": "n3", "role": "generic", "label": "Size",
                 "value": "Large", "actions": ["click", "select"]},
                {"index": 4, "region": "n4", "role": "textbox", "label": "Card",
                 "input_type": "text", "sensitive": true, "value": "4111",
                 "checked": false, "expanded": true, "value_truncated": true,
                 "actions": ["type"]},
                {"index": 5, "region": "n5", "role": "textbox", "label": "Note",
                 "input_type": "text", "value": long, "actions": ["type"]},
                {"index": 6, "region": "n6", "role": "textbox", "label": "Bio",
                 "input_type": "text", "value": "short… [truncated 9 chars]",
                 "value_truncated": true, "actions": ["type"]},
                {"index": 7, "region": "n7", "role": "tab", "label": "Reviews",
                 "selected_item": true, "aria_current": "page", "actions": ["click"]},
                {"index": 8, "region": "n8", "role": "textbox", "label": "PIN",
                 "input_type": "text", "sensitive": true, "selected_item": false,
                 "aria_current": "step", "actions": ["type"]},
                {"index": 9, "region": "n9", "role": "link", "label": "Step 2",
                 "aria_current": "s".repeat(MAX_LABEL_CHARS + 9), "actions": ["click"]},
            ]
        });
        let off = page(&answer, false).unwrap();
        for e in &off.elements {
            assert_eq!(
                (e.value.as_deref(), e.checked, e.expanded, e.value_truncated),
                (None, None, None, false),
                "{}",
                e.label
            );
            assert_eq!(
                (e.selected, e.current.as_deref()),
                (None, None),
                "{}",
                e.label
            );
        }
        let on = page(&answer, true).unwrap();
        let e = &on.elements;
        assert_eq!(e[0].checked, Some(true));
        assert_eq!(e[0].value, None);
        assert_eq!(e[1].expanded, Some(false));
        assert_eq!(e[2].value.as_deref(), Some("Large"));
        assert!(e[3].sensitive);
        assert_eq!(
            (
                e[3].value.as_deref(),
                e[3].checked,
                e[3].expanded,
                e[3].value_truncated
            ),
            (None, None, None, false)
        );
        assert_eq!(
            e[4].value.as_ref().unwrap().chars().count(),
            MAX_VALUE_CHARS + 1
        );
        assert!(e[4].value_truncated);
        assert!(e[5].value_truncated);
        assert!(!e[2].value_truncated);
        assert_eq!(
            (e[6].selected, e[6].current.as_deref()),
            (Some(true), Some("page"))
        );
        assert_eq!((e[7].selected, e[7].current.as_deref()), (None, None));
        assert_eq!(e[1].selected, None);
        assert_eq!(
            e[8].current.as_ref().unwrap().chars().count(),
            MAX_LABEL_CHARS + 1
        );
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
        assert!(page(&answer, false)
            .unwrap()
            .elements
            .iter()
            .all(|e| e.value.is_none()));
        let page = page(&answer, true).unwrap();
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
        let long = "x".repeat(MAX_KEPT_LABEL_CHARS * 3);
        let answer = json!({
            "elements": [{"index": 1, "region": "n1", "role": "button", "label": long,
                          "actions": ["a","b","c","d","e","f","g","h","i","j"]}]
        });
        let page = page(&answer, false).unwrap();
        assert_eq!(
            page.elements[0].label.chars().count(),
            MAX_KEPT_LABEL_CHARS + 1
        );
        assert_eq!(page.elements[0].actions.len(), MAX_ACTIONS);
        assert_eq!(page.total, 1);
    }

    #[test]
    fn an_answer_with_no_elements_is_the_backend_s_fault() {
        assert!(matches!(
            page(&json!({}), false),
            Err(BrowserError::Backend(_))
        ));
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
