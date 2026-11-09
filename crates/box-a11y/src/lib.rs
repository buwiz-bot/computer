//! The accessibility tree of a box screen, over AT-SPI, for computer use: an agent reads a native
//! app's buttons and fields as text, with their bounds in the pixels `/v1/cua/click` takes, and
//! presses, focuses or fills one by its `ref` instead of by pixels.
//!
//! - [`tree`]: the bounded walk and the wire shape, over any [`tree::Source`].
//! - [`bus`]: the real source, a screen's own accessibility bus (`docker/box-a11y-bus`).
//! - [`snapshots`]: the latest tree of each screen, which is what a `ref` is good for.
//!
//! The D-Bus side is the `atspi` crate's proxies (odilia-app/atspi, Apache-2.0 OR MIT) over a
//! connection made and checked here. The idea of a per-snapshot element index comes from trycua's
//! MIT Linux driver (github.com/trycua/cua, `libs/cua-driver/rust/crates/platform-linux`); no code
//! of it is copied.

pub mod bus;
pub mod snapshots;
pub mod tree;

use serde::{Deserialize, Serialize};

pub use snapshots::Snapshots;
pub use tree::{Scope, Shaped, TreeRequest};

/// The most text `set_text` takes.
pub const MAX_SET_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum A11yError {
    /// The screen has no accessibility bus box-exec may use.
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Invalid(String),
    /// A ref from an older tree, or a node that is gone.
    #[error("{0}")]
    Stale(String),
    /// The element has no way to do this.
    #[error("{0}")]
    Unsupported(String),
    /// The application said no.
    #[error("{0}")]
    Refused(String),
    /// The registry or an application failed to answer.
    #[error("{0}")]
    Backend(String),
}

/// `POST /v1/cua/tree`'s answer.
#[derive(Clone, Debug, Serialize)]
pub struct TreeResponse {
    /// This tree's number: every `ref` in it starts with it, and only the latest tree of a screen
    /// can be acted on.
    pub snapshot: u64,
    pub display: String,
    pub scope: Scope,
    #[serde(flatten)]
    pub tree: Shaped,
    pub elapsed_ms: u64,
}

/// Reads `display`'s tree from `source` and keeps it as that screen's latest. `focus_title` is the
/// title of the X window with the keyboard focus, which names the focused window best.
pub async fn read_tree<S: tree::Source>(
    source: &S,
    display: &str,
    request: &TreeRequest,
    snapshots: &Snapshots,
    focus_title: Option<&str>,
) -> Result<TreeResponse, A11yError> {
    let limits = request.limits()?;
    let started = tokio::time::Instant::now();
    let walk = tree::walk(source, &limits, focus_title).await?;
    let snapshot = snapshots.record(display, walk.handles());
    Ok(TreeResponse {
        snapshot,
        display: display.to_string(),
        scope: limits.scope,
        tree: walk.shape(snapshot),
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// [`read_tree`] on the screen's own accessibility bus.
pub async fn tree(
    display: &str,
    request: &TreeRequest,
    snapshots: &Snapshots,
    focus_title: Option<&str>,
) -> Result<TreeResponse, A11yError> {
    request.limits()?;
    let screen = bus::Screen::connect(display).await?;
    read_tree(&screen, display, request, snapshots, focus_title).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElementAction {
    /// The element's own press, click or activate action, through AT-SPI. The application says
    /// whether it took it, not whether anything happened: some widgets only act on a real click.
    Press,
    /// A real click (XTEST) in the middle of the element, where it is now.
    Click,
    /// Give it the keyboard focus.
    Focus,
    /// Replace an editable field's text with `text`.
    SetText,
}

/// `POST /v1/cua/element`.
#[derive(Clone, Debug, Deserialize)]
pub struct ElementRequest {
    #[serde(rename = "ref")]
    pub reference: String,
    pub action: ElementAction,
    pub text: Option<String>,
}

/// A point on the screen, in the pixels `/v1/cua/click` takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// What an element action did, or for `click`, where to click.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Acted {
    pub performed: Option<String>,
    pub at: Option<Point>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ElementResponse {
    pub ok: bool,
    #[serde(rename = "ref")]
    pub reference: String,
    pub action: ElementAction,
    /// For `press`: the AT-SPI action that was run (`click`, `press`, `activate`, `default`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performed: Option<String>,
    /// For `click`: where the click went.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<Point>,
}

impl ElementRequest {
    pub fn check(&self) -> Result<(), A11yError> {
        match (self.action, &self.text) {
            (ElementAction::SetText, None) => {
                Err(A11yError::Invalid("set_text needs `text`".into()))
            }
            (ElementAction::SetText, Some(text)) if text.len() > MAX_SET_TEXT_BYTES => Err(
                A11yError::Invalid(format!("text is at most {MAX_SET_TEXT_BYTES} bytes")),
            ),
            (ElementAction::SetText, Some(_)) => Ok(()),
            (_, Some(_)) => Err(A11yError::Invalid("`text` is for set_text alone".into())),
            (_, None) => Ok(()),
        }
    }
}

/// Runs an element action on `display`, on a node of its latest tree. For `click`, the answer
/// says where, and the caller clicks there before it replies.
pub async fn element(
    display: &str,
    request: &ElementRequest,
    snapshots: &Snapshots,
) -> Result<ElementResponse, A11yError> {
    request.check()?;
    let node = snapshots.resolve(display, &request.reference)?;
    let screen = bus::Screen::connect(display).await?;
    let acted = screen
        .act(&node, request.action, request.text.as_deref())
        .await?;
    Ok(ElementResponse {
        ok: true,
        reference: request.reference.trim().to_string(),
        action: request.action,
        performed: acted.performed,
        at: acted.at,
    })
}

/// Which of an element's actions `press` runs: the first press-like one by name, else the first
/// when it has no name (Chromium names none of its actions; its first is the default one), else
/// the only one.
pub fn press_action(names: &[String]) -> Option<usize> {
    const PRESS: &[&str] = &["click", "press", "activate", "jump", "toggle", "open"];
    PRESS
        .iter()
        .find_map(|want| {
            names
                .iter()
                .position(|name| name.trim().eq_ignore_ascii_case(want))
        })
        .or_else(|| {
            let first = names.first()?;
            (names.len() == 1 || first.trim().is_empty()).then_some(0)
        })
}

#[cfg(test)]
mod tests;
