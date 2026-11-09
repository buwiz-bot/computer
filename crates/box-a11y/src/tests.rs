//! The walk and its limits, over a fake screen.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::tree::{clip, walk, Bounds, Cut, Handle, Info, Limits, Node, Scope, Source};
use crate::{press_action, read_tree, A11yError, ElementAction, ElementRequest, Snapshots};
use crate::{TreeRequest, MAX_SET_TEXT_BYTES};

/// A screen of made-up nodes: each path's info and children.
#[derive(Default)]
struct Fake {
    apps: Vec<Handle>,
    nodes: HashMap<String, (Info, Vec<Handle>)>,
    /// Paths whose application never answers.
    hung: Vec<String>,
    /// Paths whose application answers with an error.
    broken: Vec<String>,
    calls: AtomicUsize,
}

fn h(path: &str) -> Handle {
    Handle {
        bus: ":1.1".into(),
        path: path.into(),
    }
}

fn shown(role: &str, name: &str) -> Info {
    Info {
        role: role.into(),
        name: name.into(),
        states: vec!["showing", "visible", "enabled"],
        ..Info::default()
    }
}

impl Fake {
    fn add(&mut self, path: &str, info: Info, children: &[&str]) {
        let mut info = info;
        info.child_count = children.len();
        self.nodes
            .insert(path.into(), (info, children.iter().map(|c| h(c)).collect()));
    }

    fn app(&mut self, path: &str) {
        self.apps.push(h(path));
    }

    async fn answer(&self, node: &Handle) -> Result<&(Info, Vec<Handle>), A11yError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.hung.contains(&node.path) {
            std::future::pending::<()>().await;
        }
        if self.broken.contains(&node.path) {
            return Err(A11yError::Backend("no".into()));
        }
        self.nodes
            .get(&node.path)
            .ok_or_else(|| A11yError::Stale("gone".into()))
    }
}

impl Source for Fake {
    async fn applications(&self) -> Result<Vec<Handle>, A11yError> {
        Ok(self.apps.clone())
    }

    async fn info(&self, node: &Handle) -> Result<Info, A11yError> {
        Ok(self.answer(node).await?.0.clone())
    }

    async fn children(&self, node: &Handle) -> Result<Vec<Handle>, A11yError> {
        Ok(self.answer(node).await?.1.clone())
    }
}

/// An app with a window holding a panel with a button and a field, and a second, inactive app.
fn desktop() -> Fake {
    let mut fake = Fake::default();
    fake.app("/app");
    fake.app("/other");
    fake.add("/app", Info::default(), &["/win"]);
    fake.add(
        "/win",
        Info {
            states: vec!["showing", "visible", "active"],
            ..shown("frame", "Files")
        },
        &["/panel"],
    );
    fake.add("/panel", shown("filler", ""), &["/ok", "/field", "/menu"]);
    fake.add(
        "/ok",
        Info {
            bounds: Some(Bounds {
                x: 10,
                y: 20,
                width: 80,
                height: 30,
            }),
            actions: vec!["click".into()],
            states: vec!["showing", "visible", "enabled", "focusable", "focused"],
            ..shown("push button", "OK")
        },
        &[],
    );
    fake.add(
        "/field",
        Info {
            text: Some("hello".into()),
            states: vec!["showing", "visible", "editable", "single-line"],
            ..shown("text", "Name")
        },
        &[],
    );
    // A closed menu: not showing, with an item under it.
    fake.add(
        "/menu",
        Info {
            states: vec!["enabled"],
            ..shown("menu", "File")
        },
        &["/menu/open"],
    );
    fake.add("/menu/open", shown("menu item", "Open"), &[]);
    fake.add("/other", Info::default(), &["/other/win"]);
    fake.add("/other/win", shown("frame", "Other"), &[]);
    fake
}

/// The defaults, but every node kept, so the shapes below are the fake's own.
fn limits() -> Limits {
    Limits {
        compact: false,
        ..TreeRequest::default().limits().unwrap()
    }
}

fn find<'a>(nodes: &'a [Node], name: &str) -> Option<&'a Node> {
    nodes.iter().find_map(|node| {
        if node.name == name {
            Some(node)
        } else {
            find(&node.children, name)
        }
    })
}

#[tokio::test]
async fn focused_window_is_the_active_one_with_its_controls() {
    let fake = desktop();
    let shaped = walk(&fake, &limits(), None).await.unwrap().shape(7);
    assert_eq!(shaped.nodes.len(), 1);
    let win = &shaped.nodes[0];
    assert_eq!((win.role.as_str(), win.name.as_str()), ("frame", "Files"));
    assert_eq!(win.reference, "7:0");
    let ok = find(&shaped.nodes, "OK").unwrap();
    assert_eq!(ok.role, "push_button");
    assert_eq!(ok.states, vec!["enabled", "focusable", "focused"]);
    assert_eq!(ok.actions, vec!["click"]);
    assert_eq!(
        ok.bounds,
        Some(Bounds {
            x: 10,
            y: 20,
            width: 80,
            height: 30
        })
    );
    let field = find(&shaped.nodes, "Name").unwrap();
    assert_eq!(field.text.as_deref(), Some("hello"));
    assert_eq!(field.states, vec!["editable"]);
    // The closed menu and what's under it are left out, and that's no truncation.
    assert!(find(&shaped.nodes, "File").is_none());
    assert!(find(&shaped.nodes, "Open").is_none());
    assert!(!shaped.truncated);
    assert_eq!(shaped.count, 4);
    assert_eq!(shaped.skipped, 0);
    assert!(shaped.reason.is_none());
}

#[tokio::test]
async fn refs_are_unique_and_in_reading_order() {
    let fake = desktop();
    let shaped = walk(&fake, &limits(), None).await.unwrap().shape(3);
    let mut refs = Vec::new();
    fn collect(nodes: &[Node], refs: &mut Vec<String>) {
        for node in nodes {
            refs.push(node.reference.clone());
            collect(&node.children, refs);
        }
    }
    collect(&shaped.nodes, &mut refs);
    assert_eq!(refs, vec!["3:0", "3:1", "3:2", "3:3"]);
}

#[tokio::test]
async fn hidden_nodes_come_when_asked_for() {
    let fake = desktop();
    let limits = Limits {
        include_hidden: true,
        ..limits()
    };
    let shaped = walk(&fake, &limits, None).await.unwrap().shape(1);
    assert!(find(&shaped.nodes, "File").is_some());
    assert!(find(&shaped.nodes, "Open").is_some());
}

#[tokio::test]
async fn desktop_scope_has_every_application() {
    let fake = desktop();
    let limits = Limits {
        scope: Scope::Desktop,
        ..limits()
    };
    let shaped = walk(&fake, &limits, None).await.unwrap().shape(1);
    assert_eq!(shaped.nodes.len(), 2);
    assert!(find(&shaped.nodes, "Other").is_some());
    assert!(find(&shaped.nodes, "OK").is_some());
}

#[tokio::test]
async fn the_node_cap_cuts_breadth_first_and_says_so() {
    let fake = desktop();
    let limits = Limits {
        max_nodes: 2,
        ..limits()
    };
    let shaped = walk(&fake, &limits, None).await.unwrap().shape(1);
    assert_eq!(shaped.count, 2);
    assert!(shaped.truncated);
    assert_eq!(shaped.truncated_by, vec![Cut::MaxNodes]);
    let panel = &shaped.nodes[0].children[0];
    assert_eq!(panel.role, "filler");
    assert!(panel.children.is_empty());
    assert_eq!(panel.children_omitted, 3);
}

#[tokio::test]
async fn the_depth_cap_keeps_the_top_and_counts_the_rest() {
    let fake = desktop();
    let limits = Limits {
        max_depth: 1,
        ..limits()
    };
    let shaped = walk(&fake, &limits, None).await.unwrap().shape(1);
    assert_eq!(shaped.count, 2);
    assert_eq!(shaped.truncated_by, vec![Cut::MaxDepth]);
    assert_eq!(shaped.nodes[0].children[0].children_omitted, 3);

    let top = Limits {
        max_depth: 0,
        ..limits
    };
    let shaped = walk(&fake, &top, None).await.unwrap().shape(1);
    assert_eq!(shaped.count, 1);
    assert_eq!(shaped.nodes[0].children_omitted, 1);
}

#[tokio::test(start_paused = true)]
async fn a_hung_application_costs_the_budget_and_returns_what_was_read() {
    let mut fake = desktop();
    fake.hung.push("/field".into());
    let limits = Limits {
        budget: Duration::from_millis(500),
        ..limits()
    };
    let started = tokio::time::Instant::now();
    let shaped = walk(&fake, &limits, None).await.unwrap().shape(1);
    assert!(started.elapsed() <= Duration::from_millis(600));
    assert!(shaped.truncated);
    assert_eq!(shaped.truncated_by, vec![Cut::Time]);
    assert!(find(&shaped.nodes, "OK").is_some());
    assert!(find(&shaped.nodes, "Name").is_none());
    assert_eq!(shaped.nodes[0].children[0].children_omitted, 1);
}

#[tokio::test]
async fn a_node_that_fails_is_skipped_not_fatal() {
    let mut fake = desktop();
    fake.broken.push("/ok".into());
    let shaped = walk(&fake, &limits(), None).await.unwrap().shape(1);
    assert_eq!(shaped.skipped, 1);
    assert!(find(&shaped.nodes, "OK").is_none());
    assert!(find(&shaped.nodes, "Name").is_some());
    assert!(!shaped.truncated);
}

#[tokio::test]
async fn nothing_published_is_an_empty_tree_with_a_reason() {
    let fake = Fake::default();
    let shaped = walk(&fake, &limits(), None).await.unwrap().shape(1);
    assert!(shaped.nodes.is_empty());
    assert!(shaped.reason.unwrap().contains("no application"));

    let mut fake = desktop();
    fake.nodes.get_mut("/win").unwrap().0.states = vec!["showing"];
    let shaped = walk(&fake, &limits(), None).await.unwrap().shape(1);
    assert!(shaped.nodes.is_empty());
    assert!(shaped.reason.unwrap().contains("keyboard focus"));
}

#[tokio::test]
async fn a_huge_child_list_is_counted_not_queued() {
    let mut fake = Fake::default();
    fake.app("/app");
    let rows: Vec<String> = (0..5000).map(|i| format!("/row/{i}")).collect();
    let row_refs: Vec<&str> = rows.iter().map(String::as_str).collect();
    fake.add("/app", Info::default(), &["/list"]);
    fake.add("/list", shown("list", "Rows"), &row_refs);
    for row in &rows {
        fake.add(row, shown("list item", row), &[]);
    }
    let limits = Limits {
        scope: Scope::Desktop,
        max_nodes: 50,
        ..limits()
    };
    let shaped = walk(&fake, &limits, None).await.unwrap().shape(1);
    assert_eq!(shaped.count, 50);
    let list = &shaped.nodes[0].children[0];
    assert_eq!(list.children.len(), 48);
    assert_eq!(list.children_omitted, 5000 - 48);
    // Only what fits was asked about.
    assert!(fake.calls.load(Ordering::Relaxed) < 120);
}

#[test]
fn strings_are_clipped_on_one_line() {
    assert_eq!(clip("  a\n\n b\tc  ", 10), "a b c");
    assert_eq!(clip("abcdef", 3), "abc…");
    assert_eq!(clip("ab cd", 3), "ab…");
    assert_eq!(clip("x\u{fffc}y", 5), "xy");
    assert_eq!(clip("", 5), "");
    let long = "é".repeat(500);
    assert_eq!(clip(&long, 160).chars().count(), 161);
}

#[tokio::test]
async fn shaping_drops_noise() {
    let mut fake = Fake::default();
    fake.app("/app");
    fake.add("/app", Info::default(), &["/win"]);
    fake.add(
        "/win",
        Info {
            states: vec!["showing", "active"],
            ..shown("frame", "W")
        },
        &["/pw", "/same", "/offscreen"],
    );
    fake.add(
        "/pw",
        Info {
            text: Some("secret".into()),
            ..shown("password text", "Password")
        },
        &[],
    );
    fake.add(
        "/same",
        Info {
            description: "Save".into(),
            text: Some("Save".into()),
            ..shown("label", "Save")
        },
        &[],
    );
    fake.add(
        "/offscreen",
        Info {
            bounds: Some(Bounds {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }),
            ..shown("label", "Zero")
        },
        &[],
    );
    let shaped = walk(&fake, &limits(), None).await.unwrap().shape(1);
    let pw = find(&shaped.nodes, "Password").unwrap();
    assert_eq!(pw.role, "password_text");
    assert!(pw.text.is_none());
    let same = find(&shaped.nodes, "Save").unwrap();
    assert!(same.description.is_empty() && same.text.is_none());
    assert!(find(&shaped.nodes, "Zero").unwrap().bounds.is_none());
    let json = serde_json::to_value(same).unwrap();
    let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, vec!["name", "ref", "role", "states"]);
}

#[tokio::test]
async fn a_node_whose_role_is_unknown_or_unread_sends_no_text() {
    let mut fake = Fake::default();
    fake.app("/app");
    fake.add("/app", Info::default(), &["/win"]);
    fake.add(
        "/win",
        Info {
            states: vec!["showing", "active"],
            ..shown("frame", "W")
        },
        &["/unknown", "/unread", "/pw"],
    );
    // `bus` reports a role it couldn't read as "unknown"; an empty one is shaped as unknown too.
    for (path, role, name) in [
        ("/unknown", "unknown", "A"),
        ("/unread", "", "B"),
        ("/pw", "password text", "C"),
    ] {
        fake.add(
            path,
            Info {
                text: Some("hunter2".into()),
                ..shown(role, name)
            },
            &[],
        );
    }
    let shaped = walk(&fake, &limits(), None).await.unwrap().shape(1);
    for name in ["A", "B", "C"] {
        assert!(find(&shaped.nodes, name).unwrap().text.is_none(), "{name}");
    }
    assert!(!serde_json::to_string(&shaped.nodes)
        .unwrap()
        .contains("hunter2"));
}

#[test]
fn limits_are_checked() {
    let bad = |req: TreeRequest| matches!(req.limits(), Err(A11yError::Invalid(_)));
    assert!(bad(TreeRequest {
        max_nodes: Some(0),
        ..TreeRequest::default()
    }));
    assert!(bad(TreeRequest {
        max_nodes: Some(100_000),
        ..TreeRequest::default()
    }));
    assert!(bad(TreeRequest {
        max_depth: Some(1000),
        ..TreeRequest::default()
    }));
    assert!(bad(TreeRequest {
        timeout_ms: Some(0),
        ..TreeRequest::default()
    }));
    let scope: TreeRequest = serde_json::from_str(r#"{"scope":"desktop"}"#).unwrap();
    assert_eq!(scope.limits().unwrap().scope, Scope::Desktop);
    assert!(serde_json::from_str::<TreeRequest>(r#"{"scope":"everything"}"#).is_err());
}

#[tokio::test]
async fn only_the_latest_tree_of_a_screen_can_be_acted_on() {
    let fake = desktop();
    let snapshots = Snapshots::starting_at(40);
    let first = read_tree(&fake, ":1", &TreeRequest::default(), &snapshots, None)
        .await
        .unwrap();
    assert_eq!(first.snapshot, 41);
    let ok = find(&first.tree.nodes, "OK").unwrap().reference.clone();
    assert_eq!(snapshots.resolve(":1", &ok).unwrap(), h("/ok"));
    // Another screen's tree, or its own refs, don't touch this one's.
    assert!(matches!(
        snapshots.resolve(":2", &ok),
        Err(A11yError::Stale(_))
    ));
    read_tree(&fake, ":2", &TreeRequest::default(), &snapshots, None)
        .await
        .unwrap();
    assert_eq!(snapshots.resolve(":1", &ok).unwrap(), h("/ok"));
    // A newer tree of the same screen makes the old refs stale.
    let second = read_tree(&fake, ":1", &TreeRequest::default(), &snapshots, None)
        .await
        .unwrap();
    assert_eq!(second.snapshot, 43);
    match snapshots.resolve(":1", &ok) {
        Err(A11yError::Stale(message)) => assert!(message.contains("read the tree again")),
        other => panic!("{other:?}"),
    }
    for bad in ["", "7", "a:b", "43:", ":1", "43:-1", "43:1:2"] {
        assert!(
            matches!(snapshots.resolve(":1", bad), Err(A11yError::Invalid(_))),
            "{bad}"
        );
    }
    assert!(matches!(
        snapshots.resolve(":1", "43:999"),
        Err(A11yError::Invalid(_))
    ));
}

#[test]
fn press_picks_a_press_like_action() {
    let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(press_action(&names(&["click"])), Some(0));
    assert_eq!(press_action(&names(&["showMenu", "press"])), Some(1));
    assert_eq!(press_action(&names(&["activate", "click"])), Some(1));
    assert_eq!(press_action(&names(&["custom"])), Some(0));
    assert_eq!(press_action(&names(&["a", "b"])), None);
    assert_eq!(press_action(&names(&["", ""])), Some(0));
    assert_eq!(press_action(&names(&["a", ""])), None);
    assert_eq!(press_action(&[]), None);
}

#[test]
fn element_requests_are_checked() {
    let req = |json: &str| serde_json::from_str::<ElementRequest>(json).unwrap();
    assert!(req(r#"{"ref":"1:2","action":"press"}"#).check().is_ok());
    assert_eq!(
        req(r#"{"ref":"1:2","action":"set_text","text":"x"}"#).action,
        ElementAction::SetText
    );
    assert!(req(r#"{"ref":"1:2","action":"set_text"}"#).check().is_err());
    assert!(req(r#"{"ref":"1:2","action":"focus","text":"x"}"#)
        .check()
        .is_err());
    let big = format!(
        r#"{{"ref":"1:2","action":"set_text","text":"{}"}}"#,
        "a".repeat(MAX_SET_TEXT_BYTES + 1)
    );
    assert!(req(&big).check().is_err());
    assert!(serde_json::from_str::<ElementRequest>(r#"{"ref":"1:2","action":"explode"}"#).is_err());
    assert!(req(r#"{"ref":"1:2","action":"click"}"#).check().is_ok());
    assert!(req(r#"{"ref":"1:2","action":"click","text":"x"}"#)
        .check()
        .is_err());
}

#[tokio::test]
async fn the_focused_x_window_names_the_focused_window() {
    let mut fake = desktop();
    // xfwm4's helper: always active, never on the screen.
    fake.app("/wm");
    fake.add("/wm", Info::default(), &["/wm/helper"]);
    fake.add(
        "/wm/helper",
        Info {
            states: vec!["active", "enabled"],
            bounds: Some(Bounds {
                x: -1000,
                y: -1000,
                width: 5,
                height: 5,
            }),
            ..Info::default()
        },
        &[],
    );
    // Chromium: on the screen, and never active.
    fake.app("/browser");
    fake.add("/browser", Info::default(), &["/browser/win"]);
    fake.add(
        "/browser/win",
        Info {
            bounds: Some(Bounds {
                x: 0,
                y: 24,
                width: 1280,
                height: 712,
            }),
            ..shown("frame", "about:blank - Chromium")
        },
        &[],
    );
    let named = walk(&fake, &limits(), Some("about:blank - Chromium"))
        .await
        .unwrap()
        .shape(1);
    assert_eq!(named.nodes[0].name, "about:blank - Chromium");
    // No title, or one nothing has: the active window on the screen, never the helper.
    for title in [None, Some("Something else"), Some("  ")] {
        let shaped = walk(&fake, &limits(), title).await.unwrap().shape(1);
        assert_eq!(shaped.nodes[0].name, "Files", "{title:?}");
    }
    fake.nodes.get_mut("/win").unwrap().0.states = vec!["showing"];
    let shaped = walk(&fake, &limits(), None).await.unwrap().shape(1);
    assert!(shaped.nodes.is_empty());
}

#[test]
fn unnamed_actions() {
    use crate::tree::action_name;
    assert_eq!(action_name(0, ""), "default");
    assert_eq!(action_name(1, ""), "");
    assert_eq!(action_name(0, "Click"), "Click");
}

#[tokio::test]
async fn unnamed_actions_are_not_listed() {
    let mut fake = desktop();
    fake.nodes.get_mut("/ok").unwrap().0.actions = vec!["".into(), "".into()];
    let shaped = walk(&fake, &limits(), None).await.unwrap().shape(1);
    assert!(find(&shaped.nodes, "OK").unwrap().actions.is_empty());
}

#[tokio::test]
async fn compact_trees_lose_their_wrappers_and_keep_their_counts() {
    let mut fake = desktop();
    // A wrapper that lost a child to the node cap hands the count to its parent.
    let compact = Limits {
        compact: true,
        ..limits()
    };
    let shaped = walk(&fake, &compact, None).await.unwrap().shape(1);
    let win = &shaped.nodes[0];
    let names: Vec<&str> = win.children.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, vec!["OK", "Name"]);
    assert_eq!(shaped.count, 3);
    // A ref still names what it did: the wrapper's number is simply not used.
    assert_eq!(win.children[0].reference, "1:2");

    let capped = Limits {
        max_nodes: 2,
        ..compact
    };
    let shaped = walk(&fake, &capped, None).await.unwrap().shape(1);
    assert!(shaped.nodes[0].children.is_empty());
    assert_eq!(shaped.nodes[0].children_omitted, 3);

    // A named, focused or actionable container stays.
    fake.nodes.get_mut("/panel").unwrap().0.name = "Toolbar".into();
    let shaped = walk(&fake, &compact, None).await.unwrap().shape(1);
    assert_eq!(shaped.nodes[0].children[0].name, "Toolbar");
}
