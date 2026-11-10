//! Boolean and optional env helpers shared by guest crates.
//!
//! `BOX_DESKTOP`, `BOX_CHROME`, `BOX_CUA`, and friends used to each parse
//! truthy strings differently. One helper: unset → `default`; empty →
//! `default`; `0` / `false` / `off` / `no` → false; anything else → true.

use std::env;

/// Parse a boolean environment variable.
pub fn env_bool(var: &str, default: bool) -> bool {
    match env::var(var) {
        Ok(raw) => parse_bool(&raw, default),
        Err(_) => default,
    }
}

fn parse_bool(raw: &str, default: bool) -> bool {
    let v = raw.trim().to_ascii_lowercase();
    if v.is_empty() {
        default
    } else {
        !matches!(v.as_str(), "0" | "false" | "off" | "no")
    }
}

/// Whether `/v1/browser/*` is served: `BOX_BROWSER` and `BOX_DESKTOP` (both default on).
/// `BOX_CHROME` is not part of it: that only says whether the main screen's Chromium starts at
/// boot. A Bot's own screen starts its browser on a pipe whatever it says (`box-screen open`), and
/// a screen with no browser on a pipe answers `503 browser_unavailable`.
pub fn browser_layer_on() -> bool {
    browser_layer_on_by(|name| env_bool(name, true))
}

/// [`browser_layer_on`] over a lookup of the box's on/off flags.
fn browser_layer_on_by(on: impl Fn(&str) -> bool) -> bool {
    on("BOX_BROWSER") && on("BOX_DESKTOP")
}

/// Non-empty env var, trimmed.
pub fn env_nonempty(var: &str) -> Option<String> {
    env::var(var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    fn with_var(key: &str, value: Option<&str>, f: impl FnOnce()) {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = env::var(key).ok();
        match value {
            Some(v) => env::set_var(key, v),
            None => env::remove_var(key),
        }
        f();
        match prev {
            Some(v) => env::set_var(key, v),
            None => env::remove_var(key),
        }
    }

    #[test]
    fn the_browser_layer_needs_the_browser_and_a_desktop_and_not_the_main_chromium() {
        let with = |off: &[&str]| browser_layer_on_by(|name| !off.contains(&name));
        assert!(with(&[]));
        assert!(
            with(&["BOX_CHROME"]),
            "BOX_CHROME=0 leaves a Bot's own screen's browser usable"
        );
        assert!(!with(&["BOX_BROWSER"]));
        assert!(!with(&["BOX_DESKTOP"]));
        assert!(!with(&["BOX_BROWSER", "BOX_CHROME"]));
    }

    #[test]
    fn false_tokens() {
        for v in ["0", "false", "OFF", "no", " False "] {
            with_var("BOX_ENV_BOOL_TEST", Some(v), || {
                assert!(!env_bool("BOX_ENV_BOOL_TEST", true), "{v}");
            });
        }
    }

    #[test]
    fn true_tokens() {
        for v in ["1", "true", "yes", "on", "2"] {
            with_var("BOX_ENV_BOOL_TEST", Some(v), || {
                assert!(env_bool("BOX_ENV_BOOL_TEST", false), "{v}");
            });
        }
    }

    #[test]
    fn unset_and_empty_use_default() {
        with_var("BOX_ENV_BOOL_TEST", None, || {
            assert!(env_bool("BOX_ENV_BOOL_TEST", true));
            assert!(!env_bool("BOX_ENV_BOOL_TEST", false));
        });
        with_var("BOX_ENV_BOOL_TEST", Some("  "), || {
            assert!(env_bool("BOX_ENV_BOOL_TEST", true));
            assert!(!env_bool("BOX_ENV_BOOL_TEST", false));
        });
    }
}
