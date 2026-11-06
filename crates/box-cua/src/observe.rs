//! What the box saw around a recipe step.
//!
//! `ok` on a receipt means "no step returned an error", and that is the only
//! thing it has ever meant. A recipe whose fixed coordinates have drifted
//! onto a different window plays every step without error and achieves
//! nothing, so the caller deciding whether to run it again could not tell a
//! run that worked from a run that did not. `ok` is left exactly as it was —
//! a cleverer boolean would make the box guess at what a recipe was *for*,
//! and a confident wrong guess is how that incident started. Instead the
//! receipt carries what the box actually saw, and the judgement stays with
//! whoever asked.
//!
//! Three facts, none of them a verdict:
//!
//! - **`target`** — the window covering the coordinate a pointer step aims
//!   at, read before the step moves anything. A taped click that lands on a
//!   different window than last time is visible here.
//! - **`focus`** — where a keystroke sent right now would go. `state: "none"`
//!   is a `type` that reached nothing at all.
//! - **`url_before` / `url_after`** — the page Chromium was showing either
//!   side of the step, so a `Return` that submitted nothing shows up as a
//!   URL that did not move.
//!
//! None of it is on by default. `observe: "off"` produces the receipt this
//! endpoint produced before the field existed, byte for byte.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::recipe::RecipeStep;
use crate::{x11, CuaConfig};

/// How much of the desktop the receipt reports back.
///
/// Split in two because the two halves cost very different things, and a
/// recipe is up to 256 steps.
///
/// `input` is X11 requests on a connection the box already keeps open and
/// forks nothing: about ten round-trips for a pointer step (the descent to
/// the window under the coordinate, then the climb to the client window that
/// names it) and about four for a `type`. That is well inside the pacing a
/// step already pays — `CLICK_GAP` alone is 12ms, and `TYPE_CHAR_MS` is 30ms
/// per character — so observing a click is an order of magnitude cheaper
/// than clicking it.
///
/// `page` adds a DevTools HTTP call either side of the steps that can
/// navigate: a loopback TCP connection and a small GET, twice per observed
/// step. Small, but real, and unlike `input` it scales into something worth
/// noticing across a long recipe. A caller that wanted to know which window
/// a click hit should not have to pay for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeObserve {
    #[default]
    Off,
    /// Window under a pointer step's target, and keyboard focus before a
    /// `type` or `key`.
    Input,
    /// Everything `input` reports, plus the page URL either side of the
    /// steps that can change it (`click`, `double_click`, `type`, `key`).
    Page,
}

/// One window, named the way the rest of the box names windows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WindowRef {
    /// X window id formatted as `wmctrl -lx` and `GET /v1/desktop/windows`
    /// format it, so a caller can line the two up without reformatting.
    pub id: String,
    /// `WM_CLASS` as `instance.class`, the same shape
    /// `GET /v1/desktop/windows` reports. Absent when no window in the
    /// ancestry claimed a class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// `_NET_WM_NAME`, falling back to `WM_NAME`. For a Chromium window this
    /// is the page title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Where the X server would deliver a keystroke sent at this moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusState {
    /// No window holds the focus. The X server discards keyboard events, so
    /// a `type` here reached nothing whatsoever.
    None,
    /// Focus follows the pointer; `window` is what was under it.
    PointerRoot,
    /// The root window holds the focus, which is where a window manager
    /// parks it when no client will take it. Nothing on this desktop selects
    /// key events on the root, so this is "nowhere" too.
    Root,
    /// A client window holds the focus and is reported in `window`.
    Window,
}

/// The keyboard focus as the box read it just before sending keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Focus {
    pub state: FocusState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowRef>,
}

/// What the box saw around one step.
///
/// Every field is optional and every absent field means *not observed*, never
/// *nothing was there*. That distinction is the whole point: an empty string
/// for "the window under the click" would be the same class of lie as an
/// empty stdout that might or might not have been thrown away.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StepObservation {
    /// The window covering this step's target coordinate, read before the
    /// step ran. Absent when the step aims at no coordinate, or when nothing
    /// but the root window was mapped there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<WindowRef>,
    /// Where the keys were about to go. Only read for `type` and `key`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<Focus>,
    /// The page Chromium was showing before the step. `observe: "page"` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_before: Option<String>,
    /// The page Chromium was showing once the step returned, after any
    /// `settle` wait. Read immediately: a browser navigation is not instant,
    /// so a `Return` that *did* navigate often still shows the old URL here
    /// and the new one in the next step's `url_before`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_after: Option<String>,
    /// Wall time this step spent looking rather than acting. Reported so the
    /// cost of `observe` is measurable from a receipt instead of taken on
    /// trust; it is not counted in the step's `ms`, for the same reason a
    /// per-step screenshot is not.
    pub observe_ms: u64,
}

/// What the box will look at for one step, decided from the step alone.
///
/// `None` means the box did not look, because there was nothing about the
/// step to look at — a `wait` has no target coordinate and sends no keys.
/// That is deliberately not the same as an `observed` block whose fields are
/// all missing, which means the box looked and the X server or Chromium did
/// not answer.
pub(crate) struct StepWatch {
    target: Option<(i32, i32)>,
    focus: bool,
    page: bool,
}

pub(crate) fn watch(mode: RecipeObserve, step: &RecipeStep) -> Option<StepWatch> {
    if mode == RecipeObserve::Off {
        return None;
    }
    let page = mode == RecipeObserve::Page;
    let watch = match step {
        // A click and a double-click are the steps that carry a taped
        // coordinate into a desktop that has moved on, so they get the
        // page as well as the window.
        RecipeStep::Click { x, y, .. } | RecipeStep::DoubleClick { x, y, .. } => StepWatch {
            target: Some((*x, *y)),
            focus: false,
            page,
        },
        RecipeStep::Move { x, y }
        | RecipeStep::Press { x, y, .. }
        | RecipeStep::Scroll { x, y, .. } => StepWatch {
            target: Some((*x, *y)),
            focus: false,
            page: false,
        },
        // The grab point is the one that has to be right; where a drag ends
        // is wherever the recipe dragged to.
        RecipeStep::Drag { x1, y1, .. } => StepWatch {
            target: Some((*x1, *y1)),
            focus: false,
            page: false,
        },
        // A release with no coordinate lets go wherever the pointer already
        // is, which the step that put it there has already reported.
        RecipeStep::Release { x, y, .. } => StepWatch {
            target: match (x, y) {
                (Some(x), Some(y)) => Some((*x, *y)),
                _ => return None,
            },
            focus: false,
            page: false,
        },
        RecipeStep::Type { .. } | RecipeStep::Key { .. } => StepWatch {
            target: None,
            focus: true,
            page,
        },
        RecipeStep::Wait { .. } | RecipeStep::Screenshot {} | RecipeStep::ResetDesktop {} => {
            return None
        }
    };
    Some(watch)
}

impl StepWatch {
    /// Read what the step is about to act on, before it acts.
    ///
    /// Before rather than after, because a click that raises or maps a
    /// window has already changed the answer by the time it returns, and the
    /// question is what the click landed on.
    pub(crate) async fn before(&self, config: &CuaConfig) -> StepObservation {
        let started = Instant::now();
        let mut obs = StepObservation::default();
        if let Some((x, y)) = self.target {
            obs.target = x11::observe_window_at(config, x, y).await;
        }
        if self.focus {
            obs.focus = x11::observe_focus(config).await;
        }
        if self.page {
            obs.url_before = page_url(config);
        }
        obs.observe_ms = started.elapsed().as_millis() as u64;
        obs
    }

    /// Read the page again once the step, and any `settle` wait after it,
    /// are done.
    pub(crate) async fn after(&self, config: &CuaConfig, obs: &mut StepObservation) {
        if !self.page {
            return;
        }
        let started = Instant::now();
        obs.url_after = page_url(config);
        obs.observe_ms = obs
            .observe_ms
            .saturating_add(started.elapsed().as_millis() as u64);
    }
}

/// The page in front of this screen's own browser, as its extension last said. `None` means nothing
/// has reported: that's not a page with a blank address, so the field is left out.
fn page_url(config: &CuaConfig) -> Option<String> {
    box_desktop::page_url(&config.display)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click() -> RecipeStep {
        RecipeStep::Click {
            x: 529,
            y: 126,
            button: None,
        }
    }

    #[test]
    fn off_looks_at_nothing() {
        assert!(watch(RecipeObserve::Off, &click()).is_none());
        assert!(watch(
            RecipeObserve::Off,
            &RecipeStep::Type {
                text: "kabisado".into()
            }
        )
        .is_none());
    }

    #[test]
    fn a_click_is_watched_at_its_own_coordinate() {
        let w = watch(RecipeObserve::Input, &click()).expect("a click has a target");
        assert_eq!(w.target, Some((529, 126)));
        assert!(!w.focus);
        assert!(!w.page, "input mode must not reach for DevTools");
        let w = watch(RecipeObserve::Page, &click()).expect("a click has a target");
        assert!(w.page);
    }

    #[test]
    fn a_drag_is_watched_where_it_grabs() {
        let w = watch(
            RecipeObserve::Input,
            &RecipeStep::Drag {
                x1: 10,
                y1: 20,
                x2: 300,
                y2: 400,
                button: None,
            },
        )
        .expect("a drag has a grab point");
        assert_eq!(w.target, Some((10, 20)));
    }

    #[test]
    fn typing_watches_focus_not_a_coordinate() {
        let w = watch(
            RecipeObserve::Page,
            &RecipeStep::Type {
                text: "kabisado".into(),
            },
        )
        .expect("a type has somewhere for the text to go");
        assert!(w.focus);
        assert_eq!(w.target, None);
        assert!(w.page);
    }

    #[test]
    fn steps_with_nothing_to_see_are_not_claimed_as_observed() {
        for step in [
            RecipeStep::Wait { ms: 10 },
            RecipeStep::Screenshot {},
            RecipeStep::ResetDesktop {},
            RecipeStep::Release {
                x: None,
                y: None,
                button: None,
                path: None,
            },
        ] {
            assert!(
                watch(RecipeObserve::Page, &step).is_none(),
                "{} has nothing to observe and must not carry an empty block",
                step.op_name()
            );
        }
    }

    #[test]
    fn an_empty_observation_still_says_the_box_looked() {
        // All four facts missing is itself a fact: the box asked and got no
        // answer. It has to survive serialization as an object, not vanish.
        let json = serde_json::to_string(&StepObservation::default()).unwrap();
        assert_eq!(json, r#"{"observe_ms":0}"#);
    }

    #[test]
    fn nothing_observed_is_never_reported_as_nothing_there() {
        let obs = StepObservation {
            target: Some(WindowRef {
                id: "0x02a00003".into(),
                class: Some("chromium.Chromium".into()),
                title: None,
            }),
            focus: Some(Focus {
                state: FocusState::None,
                window: None,
            }),
            url_before: None,
            url_after: None,
            observe_ms: 3,
        };
        let json = serde_json::to_string(&obs).unwrap();
        assert!(!json.contains("title"), "an unread title is absent: {json}");
        assert!(!json.contains("url_"), "an unread url is absent: {json}");
        assert!(json.contains(r#""state":"none""#), "{json}");
    }

    #[test]
    fn mode_names_on_the_wire() {
        assert_eq!(
            serde_json::from_str::<RecipeObserve>(r#""page""#).unwrap(),
            RecipeObserve::Page
        );
        assert_eq!(
            serde_json::from_str::<RecipeObserve>(r#""input""#).unwrap(),
            RecipeObserve::Input
        );
        assert_eq!(RecipeObserve::default(), RecipeObserve::Off);
    }
}
