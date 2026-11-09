//! A bounded snapshot of an accessibility tree, from anything that can answer three questions about
//! it ([`Source`]): which applications there are, what one node is, and what its children are.
//!
//! The walk is breadth first, so a cut tree still shows every window's top before any one window's
//! depths. Everything is bounded: the node count, the depth, the time, and every string. A cut is
//! reported (`truncated`, `truncated_by`, and `children_omitted` on the node that lost children),
//! never an error.

use std::collections::{BTreeSet, VecDeque};
use std::future::Future;
use std::time::Duration;

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use tokio::time::{timeout_at, Instant};

use crate::A11yError;

pub const DEFAULT_MAX_NODES: usize = 400;
pub const MAX_NODES: usize = 2000;
pub const DEFAULT_MAX_DEPTH: usize = 12;
pub const MAX_DEPTH: usize = 40;
pub const DEFAULT_TIMEOUT_MS: u64 = 3000;
pub const MAX_TIMEOUT_MS: u64 = 10_000;
/// Characters kept of a name or a description.
pub const MAX_NAME_CHARS: usize = 160;
/// Characters kept of a node's text (a field's contents, a paragraph).
pub const MAX_TEXT_CHARS: usize = 400;
/// Actions kept per node, and characters per action name.
pub const MAX_ACTIONS: usize = 8;
pub const MAX_ACTION_CHARS: usize = 32;
/// Nodes asked about at once.
const CONCURRENCY: usize = 16;

/// What a tree is of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The window that has the focus (AT-SPI's `active` window), and what's in it.
    #[default]
    FocusedWindow,
    /// Every application on the screen, each with its windows.
    Desktop,
}

/// `POST /v1/cua/tree`. Every field is optional.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TreeRequest {
    pub scope: Option<Scope>,
    /// Levels below the top nodes (an application, or the focused window). 0 is the top alone.
    pub max_depth: Option<usize>,
    pub max_nodes: Option<usize>,
    /// The walk's time budget. What's read by then is returned, with `truncated_by: ["time"]`.
    pub timeout_ms: Option<u64>,
    /// Nodes that are neither showing nor visible, and what's under them, are left out unless this
    /// is true: a closed menu, a tab behind another.
    pub include_hidden: Option<bool>,
    /// Nameless containers with nothing to act on (`panel`, `filler`, `section`, …) give their
    /// place to their children, unless this is false. On by default: Chromium wraps every control
    /// in a few of them.
    pub compact: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub scope: Scope,
    pub max_depth: usize,
    pub max_nodes: usize,
    pub budget: Duration,
    pub include_hidden: bool,
    pub compact: bool,
}

impl TreeRequest {
    pub fn limits(&self) -> Result<Limits, A11yError> {
        let max_nodes = self.max_nodes.unwrap_or(DEFAULT_MAX_NODES);
        if !(1..=MAX_NODES).contains(&max_nodes) {
            return Err(A11yError::Invalid(format!(
                "max_nodes is 1 to {MAX_NODES}, not {max_nodes}"
            )));
        }
        let max_depth = self.max_depth.unwrap_or(DEFAULT_MAX_DEPTH);
        if max_depth > MAX_DEPTH {
            return Err(A11yError::Invalid(format!(
                "max_depth is 0 to {MAX_DEPTH}, not {max_depth}"
            )));
        }
        let timeout_ms = self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(A11yError::Invalid(format!(
                "timeout_ms is 1 to {MAX_TIMEOUT_MS}, not {timeout_ms}"
            )));
        }
        Ok(Limits {
            scope: self.scope.unwrap_or_default(),
            max_depth,
            max_nodes,
            budget: Duration::from_millis(timeout_ms),
            include_hidden: self.include_hidden.unwrap_or(false),
            compact: self.compact.unwrap_or(true),
        })
    }
}

/// Where a node lives: its application's bus name and its object path. Good for as long as the
/// application keeps the object.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Handle {
    pub bus: String,
    pub path: String,
}

/// A rectangle in screen pixels, the space `/v1/cua/click` takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// One node as its application reports it, before any shaping.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    /// AT-SPI's role name: `push button`, `text`, `frame`.
    pub role: String,
    pub name: String,
    pub description: String,
    /// AT-SPI's state names: `focused`, `showing`, `multi-line`.
    pub states: Vec<&'static str>,
    pub bounds: Option<Bounds>,
    pub actions: Vec<String>,
    pub text: Option<String>,
    pub child_count: usize,
}

impl Info {
    fn has(&self, state: &str) -> bool {
        self.states.contains(&state)
    }

    /// Whether it, and so what's under it, is left out when hidden nodes are.
    fn hidden(&self) -> bool {
        self.has("defunct") || !(self.has("showing") || self.has("visible"))
    }
}

/// What a tree is read from: the screen's AT-SPI bus ([`crate::bus::Screen`]), or a fake in tests.
pub trait Source: Sync {
    /// The applications' root nodes.
    fn applications(&self) -> impl Future<Output = Result<Vec<Handle>, A11yError>> + Send;
    fn info(&self, node: &Handle) -> impl Future<Output = Result<Info, A11yError>> + Send;
    fn children(
        &self,
        node: &Handle,
    ) -> impl Future<Output = Result<Vec<Handle>, A11yError>> + Send;
}

/// A limit that cut the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cut {
    MaxNodes,
    MaxDepth,
    Time,
}

struct Pending {
    handle: Handle,
    depth: usize,
    parent: Option<usize>,
}

struct Visited {
    handle: Handle,
    info: Info,
    children: Vec<usize>,
    omitted: usize,
}

/// A tree as read, before it has a snapshot number.
pub struct Walk {
    compact: bool,
    visited: Vec<Visited>,
    roots: Vec<usize>,
    cuts: BTreeSet<Cut>,
    skipped: usize,
    reason: Option<String>,
}

enum Visit {
    Done(Info, Option<Vec<Handle>>),
    Failed,
    OutOfTime,
}

async fn visit<S: Source>(
    source: &S,
    node: &Handle,
    expand: bool,
    include_hidden: bool,
    top: bool,
    deadline: Instant,
) -> Visit {
    let read = async {
        let info = source.info(node).await?;
        let pruned = !include_hidden && !top && info.hidden();
        let children = if expand && info.child_count > 0 && !pruned {
            Some(source.children(node).await?)
        } else {
            None
        };
        Ok::<_, A11yError>((info, children))
    };
    match timeout_at(deadline, read).await {
        Ok(Ok((info, children))) => Visit::Done(info, children),
        Ok(Err(_)) => Visit::Failed,
        Err(_) => Visit::OutOfTime,
    }
}

/// Reads the tree `limits` asks for from `source`. `focus_title` is the title of the X window that
/// has the keyboard focus, when it's known: see [`focused_window`].
pub async fn walk<S: Source>(
    source: &S,
    limits: &Limits,
    focus_title: Option<&str>,
) -> Result<Walk, A11yError> {
    let deadline = Instant::now() + limits.budget;
    let mut walk = Walk {
        compact: limits.compact,
        visited: Vec::new(),
        roots: Vec::new(),
        cuts: BTreeSet::new(),
        skipped: 0,
        reason: None,
    };
    let apps = timeout_at(deadline, source.applications())
        .await
        .map_err(|_| {
            A11yError::Backend("the accessibility registry did not answer in time".into())
        })??;
    if apps.is_empty() {
        walk.reason = Some("no application on this screen publishes an accessibility tree".into());
        return Ok(walk);
    }
    let roots = match limits.scope {
        Scope::Desktop => apps,
        Scope::FocusedWindow => match focused_window(source, &apps, focus_title, deadline).await {
            Some(window) => vec![window],
            None => {
                walk.reason = Some(format!(
                    "no window of the {} applications that publish a tree has the keyboard focus; scope \"desktop\" has them all",
                    apps.len()
                ));
                if Instant::now() >= deadline {
                    walk.cuts.insert(Cut::Time);
                }
                return Ok(walk);
            }
        },
    };

    let mut queue: VecDeque<Pending> = roots
        .into_iter()
        .map(|handle| Pending {
            handle,
            depth: 0,
            parent: None,
        })
        .collect();
    while !queue.is_empty() {
        let room = limits.max_nodes - walk.visited.len();
        if room == 0 {
            walk.cuts.insert(Cut::MaxNodes);
            break;
        }
        if Instant::now() >= deadline {
            walk.cuts.insert(Cut::Time);
            break;
        }
        let take = queue.len().min(room).min(CONCURRENCY);
        let batch: Vec<Pending> = queue.drain(..take).collect();
        let results = join_all(batch.iter().map(|pending| {
            visit(
                source,
                &pending.handle,
                pending.depth < limits.max_depth,
                limits.include_hidden,
                pending.depth == 0,
                deadline,
            )
        }))
        .await;
        for (pending, result) in batch.into_iter().zip(results) {
            let (info, children) = match result {
                Visit::Done(info, children) => (info, children),
                Visit::Failed => {
                    walk.skipped += 1;
                    continue;
                }
                Visit::OutOfTime => {
                    walk.cuts.insert(Cut::Time);
                    walk.omit(pending.parent);
                    continue;
                }
            };
            if !limits.include_hidden && pending.depth > 0 && info.hidden() {
                continue;
            }
            let index = walk.visited.len();
            match pending.parent {
                Some(parent) => walk.visited[parent].children.push(index),
                None => walk.roots.push(index),
            }
            let mut omitted = 0;
            match children {
                Some(children) => {
                    // More than the cap can never be read: count them, don't queue them.
                    let keep = children.len().min(limits.max_nodes);
                    omitted = children.len() - keep;
                    if omitted > 0 {
                        walk.cuts.insert(Cut::MaxNodes);
                    }
                    queue.extend(children.into_iter().take(keep).map(|handle| Pending {
                        handle,
                        depth: pending.depth + 1,
                        parent: Some(index),
                    }));
                }
                None if info.child_count > 0 && pending.depth >= limits.max_depth => {
                    omitted = info.child_count;
                    walk.cuts.insert(Cut::MaxDepth);
                }
                None => {}
            }
            walk.visited.push(Visited {
                handle: pending.handle,
                info,
                children: Vec::new(),
                omitted,
            });
        }
    }
    for pending in std::mem::take(&mut queue) {
        walk.omit(pending.parent);
    }
    Ok(walk)
}

/// The window with the keyboard focus. X knows which one that is better than AT-SPI does: Chromium
/// never marks its frame `active`, and xfwm4 keeps an offscreen helper window that always is. So
/// the window named as the focused X window is titled wins, then one AT-SPI calls active. A window
/// off the screen, or too small to hold anything, is never it.
async fn focused_window<S: Source>(
    source: &S,
    apps: &[Handle],
    focus_title: Option<&str>,
    deadline: Instant,
) -> Option<Handle> {
    let windows: Vec<Handle> = join_all(apps.iter().map(|app| async move {
        timeout_at(deadline, source.children(app))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
    }))
    .await
    .into_iter()
    .flatten()
    .collect();
    let infos = join_all(
        windows
            .iter()
            .map(|window| timeout_at(deadline, source.info(window))),
    )
    .await;
    let candidates: Vec<(Handle, Info)> = windows
        .into_iter()
        .zip(infos)
        .filter_map(|(window, info)| Some((window, info.ok()?.ok()?)))
        .filter(|(_, info)| on_screen(info))
        .collect();
    let title = focus_title.map(str::trim).filter(|title| !title.is_empty());
    if let Some(title) = title {
        if let Some((window, _)) = candidates
            .iter()
            .find(|(_, info)| info.name.trim() == title)
        {
            return Some(window.clone());
        }
    }
    candidates
        .into_iter()
        .find(|(_, info)| info.has("active"))
        .map(|(window, _)| window)
}

/// Whether a window could be what someone is looking at. One with no bounds might be.
fn on_screen(info: &Info) -> bool {
    const SMALLEST: i32 = 16;
    info.bounds.is_none_or(|b| {
        b.width >= SMALLEST
            && b.height >= SMALLEST
            && b.x.saturating_add(b.width) > 0
            && b.y.saturating_add(b.height) > 0
    })
}

impl Walk {
    fn omit(&mut self, parent: Option<usize>) {
        if let Some(parent) = parent {
            self.visited[parent].omitted += 1;
        }
    }

    /// Where each node lives, in `ref` order: what a snapshot keeps.
    pub fn handles(&self) -> Vec<Handle> {
        self.visited.iter().map(|v| v.handle.clone()).collect()
    }

    /// The tree, with each node's `ref` in snapshot `snapshot`.
    pub fn shape(self, snapshot: u64) -> Shaped {
        let total = self.visited.len();
        // What each node becomes in its parent's children: itself, or, when it's a container
        // compacted away, its own children, and the children it lost to a limit.
        let mut built: Vec<Option<(Vec<Node>, usize)>> = Vec::with_capacity(total);
        built.resize_with(total, || None);
        let mut count = 0;
        // Children always come after their parent, so building from the end has every child ready.
        for (index, visited) in self.visited.into_iter().enumerate().rev() {
            let mut children = Vec::new();
            let mut omitted = visited.omitted;
            for &child in &visited.children {
                if let Some((nodes, lost)) = built[child].take() {
                    children.extend(nodes);
                    omitted += lost;
                }
            }
            let node = Node::shape(
                format!("{snapshot}:{index}"),
                visited.info,
                children,
                omitted,
            );
            let top = self.roots.contains(&index);
            built[index] = Some(if self.compact && !top && node.is_wrapper() {
                (node.children, node.children_omitted)
            } else {
                count += 1;
                (vec![node], 0)
            });
        }
        let nodes = self
            .roots
            .iter()
            .filter_map(|&root| built[root].take())
            .flat_map(|(nodes, _)| nodes)
            .collect();
        Shaped {
            nodes,
            count,
            truncated: !self.cuts.is_empty(),
            truncated_by: self.cuts.into_iter().collect(),
            skipped: self.skipped,
            reason: self.reason,
        }
    }
}

/// A tree, shaped for the wire.
#[derive(Clone, Debug, Serialize)]
pub struct Shaped {
    pub nodes: Vec<Node>,
    /// Nodes in `nodes`, at every depth. `max_nodes` counts the nodes read, compacted ones too.
    pub count: usize,
    pub truncated: bool,
    pub truncated_by: Vec<Cut>,
    /// Nodes whose application failed to answer about them, left out with what's under them.
    pub skipped: usize,
    /// Why `nodes` is empty, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One node of a tree.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Node {
    /// `<snapshot>:<n>`: this node in this tree, for `POST /v1/cua/element`.
    #[serde(rename = "ref")]
    pub reference: String,
    /// `push_button`, `text`, `frame`, …: AT-SPI's role, in snake case.
    pub role: String,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// What a text node holds, when it isn't its name. A password field's never is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub states: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
    /// Children it has that a limit left out of `children`.
    #[serde(skip_serializing_if = "is_zero")]
    pub children_omitted: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The states worth a caller's tokens, as AT-SPI names them and as they're sent. `showing` and
/// `visible` are left out: a hidden node is too, unless asked for.
const STATES: &[(&str, &str)] = &[
    ("active", "active"),
    ("busy", "busy"),
    ("checkable", "checkable"),
    ("checked", "checked"),
    ("collapsed", "collapsed"),
    ("editable", "editable"),
    ("enabled", "enabled"),
    ("expandable", "expandable"),
    ("expanded", "expanded"),
    ("focusable", "focusable"),
    ("focused", "focused"),
    ("has-popup", "has_popup"),
    ("indeterminate", "indeterminate"),
    ("invalid-entry", "invalid_entry"),
    ("modal", "modal"),
    ("multi-line", "multi_line"),
    ("pressed", "pressed"),
    ("read-only", "read_only"),
    ("required", "required"),
    ("selectable", "selectable"),
    ("selected", "selected"),
];

/// Roles that only group what's in them.
const WRAPPERS: &[&str] = &[
    "panel",
    "filler",
    "section",
    "grouping",
    "redundant_object",
    "unknown",
    "invalid",
];

impl Node {
    /// A container with nothing of its own to say or do.
    fn is_wrapper(&self) -> bool {
        WRAPPERS.contains(&self.role.as_str())
            && self.name.is_empty()
            && self.description.is_empty()
            && self.text.is_none()
            && self.actions.is_empty()
            && self
                .states
                .iter()
                .all(|s| matches!(*s, "enabled" | "focusable"))
    }

    fn shape(reference: String, info: Info, children: Vec<Node>, children_omitted: usize) -> Self {
        let role = snake(&info.role);
        let name = clip(&info.name, MAX_NAME_CHARS);
        let mut description = clip(&info.description, MAX_NAME_CHARS);
        if description == name {
            description.clear();
        }
        let text = info
            .text
            .filter(|_| role != "password_text")
            .map(|text| clip(&text, MAX_TEXT_CHARS))
            .filter(|text| !text.is_empty() && *text != name);
        let states = STATES
            .iter()
            .filter(|(atspi, _)| info.states.contains(atspi))
            .map(|&(_, wire)| wire)
            .collect();
        let bounds = info.bounds.filter(|b| b.width > 0 && b.height > 0);
        let actions = info
            .actions
            .iter()
            .map(|action| clip(action, MAX_ACTION_CHARS))
            .filter(|action| !action.is_empty())
            .take(MAX_ACTIONS)
            .collect();
        Self {
            reference,
            role,
            name,
            description,
            text,
            states,
            bounds,
            actions,
            children,
            children_omitted,
        }
    }
}

/// An action's name as `press` reports it. An unnamed first action is the element's default one
/// (Chromium names none of its actions). Unnamed actions aren't listed in a tree: Chromium gives
/// every node one.
pub fn action_name(index: usize, name: &str) -> String {
    let name = clip(name, MAX_ACTION_CHARS);
    if name.is_empty() && index == 0 {
        return "default".into();
    }
    name
}

/// `push button` → `push_button`.
fn snake(role: &str) -> String {
    let role = clip(role, MAX_ACTION_CHARS);
    if role.is_empty() {
        return "unknown".into();
    }
    role.chars()
        .map(|c| match c {
            ' ' | '-' => '_',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

/// At most `max` characters of `text`, on one line, with `…` where it was cut. Embedded-object
/// characters (U+FFFC, where a child sits in its parent's text) and control characters go.
pub fn clip(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut kept = 0;
    let mut space = false;
    for c in text.trim().chars() {
        if c == '\u{fffc}' {
            continue;
        }
        if c.is_whitespace() || c.is_control() {
            space = !out.is_empty();
            continue;
        }
        if kept == max {
            out.push('…');
            return out;
        }
        if space {
            if kept + 1 == max {
                out.push('…');
                return out;
            }
            out.push(' ');
            kept += 1;
            space = false;
        }
        out.push(c);
        kept += 1;
    }
    out
}
