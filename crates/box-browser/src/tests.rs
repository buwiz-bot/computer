//! box-browser against a fake [`Tools`]: what ultra-instinct would answer, scripted, and every call
//! box-browser made, kept. A few run ultra-instinct's own `Server` on a replayed page.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{json, Value};

use crate::policy::Rules;
use crate::seam::{
    BlockedBy, GoalBounds, GoalEnd, GoalReport, GoalStep, PolicyLists, ToolFailure, Tools,
};
use crate::*;

#[derive(Default)]
struct World {
    url: String,
    elements: Vec<Value>,
    calls: Vec<(String, Value)>,
    policies: Vec<PolicyLists>,
    goal: Option<GoalReport>,
    goals: Vec<(String, u32)>,
    /// What the next call answers instead, once.
    fail_next: Option<ToolFailure>,
    /// What the next policy set answers instead, once.
    fail_policy: Option<ToolFailure>,
    /// The region the next press lands on, instead of the one at its index.
    drift_to: Option<String>,
    shot: Vec<u8>,
    /// The next call panics, once.
    panic_next: bool,
    /// Labels the fake guard wants confirmed besides the policy's.
    extra_confirm: Vec<String>,
    /// States read so far: the latest is the snapshot a press must name.
    snapshots: u64,
}

type Shared = Arc<Mutex<World>>;

struct Fake(Shared);

fn button(index: u64, region: &str, label: &str) -> Value {
    json!({"index": index, "region": region, "role": "button", "label": label,
           "actions": ["click"], "rect": {"x": 10.0 * index as f64, "y": 20, "w": 80, "h": 30}})
}

impl Tools for Fake {
    fn call(&mut self, name: &str, args: &Value) -> Result<Value, ToolFailure> {
        let mut w = self.0.lock().unwrap();
        w.calls.push((name.to_string(), args.clone()));
        if let Some(fail) = w.fail_next.take() {
            return Err(fail);
        }
        if std::mem::take(&mut w.panic_next) {
            drop(w);
            panic!("the fake panics, as asked");
        }
        let at = |w: &World| -> Result<Value, ToolFailure> {
            let index = args["index"].as_u64().unwrap();
            w.elements
                .iter()
                .find(|e| e["index"] == index)
                .cloned()
                .ok_or_else(|| ToolFailure::Invalid(format!("no interactive element at {index}")))
        };
        Ok(match name {
            "browser_get_state" => {
                assert_eq!(args["format"], "json");
                w.snapshots += 1;
                // as ultra-instinct does: what a field holds only with `values: true`; the fake
                // sends a sensitive one's too, for box-browser to drop
                let mut elements = w.elements.clone();
                if args.get("values") != Some(&json!(true)) {
                    for el in &mut elements {
                        let el = el.as_object_mut().unwrap();
                        for key in [
                            "value",
                            "value_truncated",
                            "checked",
                            "expanded",
                            "selected_item",
                            "aria_current",
                        ] {
                            el.remove(key);
                        }
                    }
                }
                json!({"url": w.url, "title": "Shop", "interactive_elements": elements.len(),
                       "elements": elements, "snapshot": format!("s{}", w.snapshots)})
            }
            "browser_click"
            | "browser_type"
            | "browser_select_dropdown"
            | "browser_send_key"
            | "browser_scroll_to"
            | "browser_focus" => {
                // `require_snapshot(true)`: a press names the state its index was read in
                if args["snapshot"] != json!(format!("s{}", w.snapshots)) {
                    return Err(ToolFailure::NotAllowed(format!(
                        "snapshot_required: {name} named {}, the latest is s{}",
                        args["snapshot"], w.snapshots
                    )));
                }
                // a click is ultra-instinct's default, a real pointer press: box-browser never
                // asks for its DOM script click (`mouse: false`)
                assert!(args.get("mouse").is_none(), "{name} {args}");
                let el = at(&w)?;
                // ultra-instinct's own guard, as the fake's: the policy last set, and this call's
                // `confirm`, decide.
                let mut policy = w
                    .policies
                    .last()
                    .cloned()
                    .unwrap_or_default()
                    .to_policy()
                    .unwrap();
                if let Some(label) = args["confirm"].as_str() {
                    policy = policy.with_confirmed(label);
                }
                let needs = |label: &str| {
                    policy.confirm_pattern(label).is_some()
                        || (w.extra_confirm.iter().any(|l| l == label)
                            && !policy.confirmed.iter().any(|c| c == label))
                };
                let label = el["label"].as_str().unwrap();
                let key = args["key"].as_str().unwrap_or_default();
                let activates = matches!(key, "Enter" | "Shift+Enter" | "Space");
                let refused = match name {
                    "browser_click" => needs(label).then(|| label.to_string()),
                    // an Enter or a Space presses the control, or sends its form: the fake's form
                    // is the page, and its submit button the first one a confirm rule names
                    "browser_send_key" if activates => {
                        needs(label).then(|| label.to_string()).or_else(|| {
                            w.elements
                                .iter()
                                .filter_map(|e| e["label"].as_str())
                                .find(|l| Rules::default().needs_confirm(l))
                                .filter(|l| needs(l))
                                .map(str::to_string)
                        })
                    }
                    _ => None,
                };
                if let Some(label) = refused {
                    return Err(ToolFailure::NeedsConfirm { label });
                }
                let region = w
                    .drift_to
                    .take()
                    .unwrap_or_else(|| el["region"].as_str().unwrap().into());
                json!({"acted": name, "index": args["index"], "region": region, "ticket": 41})
            }
            "browser_navigate" => {
                w.url = args["url"].as_str().unwrap().into();
                json!({"navigated": w.url})
            }
            "browser_list_tabs" => json!({"tabs": [
                {"tab_id": "T1", "url": w.url, "title": "Shop", "ws_url": "ws://never/shown"},
                {"tab_id": "T2", "url": "about:blank", "title": ""}]}),
            "browser_new_tab" => json!({"tab_id": "T3", "url": args["url"], "new_tab": true}),
            "browser_switch_tab" => json!({"active_tab": args["tab_id"]}),
            "browser_close_tab" => json!({"closed": true, "tab_id": args["tab_id"]}),
            "browser_wait" => json!({"seen": true, "after_ms": 5, "looks": 1, "url": w.url}),
            "browser_screenshot" => json!({"format": "png",
                "base64": base64::engine::general_purpose::STANDARD.encode(&w.shot)}),
            "browser_state_save" => {
                let path = args["path"].as_str().unwrap();
                std::fs::write(
                    path,
                    br#"{"version":1,"cookies":[{"name":"sid","value":"s3cret"}]}"#,
                )
                .unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
                json!({"path": path, "cookies": 1, "origins": [{"origin": "https://shop.example", "local": 2, "session": 0}]})
            }
            "browser_state_load" => {
                let path = args["path"].as_str().unwrap();
                assert!(std::fs::read(path).is_ok());
                json!({"cookies": 1, "pending_origins": ["https://other.example"]})
            }
            other => panic!("box-browser called {other}"),
        })
    }

    fn set_policy(&mut self, policy: &PolicyLists) -> Result<(), ToolFailure> {
        policy.to_policy()?;
        let mut w = self.0.lock().unwrap();
        if let Some(fail) = w.fail_policy.take() {
            return Err(fail);
        }
        w.policies.push(policy.clone());
        Ok(())
    }

    fn run_goal(&mut self, goal: &str, bounds: &GoalBounds) -> Result<GoalReport, ToolFailure> {
        let mut w = self.0.lock().unwrap();
        w.goals.push((goal.to_string(), bounds.max_steps));
        Ok(w.goal.clone().expect("a goal report"))
    }

    fn tabs(&mut self) -> Result<Vec<seam::TabInfo>, ToolFailure> {
        let w = self.0.lock().unwrap();
        w.calls.len(); // the tab list doesn't go through a tool
        Ok(vec![
            seam::TabInfo {
                id: "T1".into(),
                url: w.url.clone(),
                title: "Shop".into(),
            },
            seam::TabInfo {
                id: "T2".into(),
                url: "about:blank".into(),
                title: String::new(),
            },
        ])
    }

    fn open_tab(&mut self, url: &str) -> Result<String, ToolFailure> {
        self.0
            .lock()
            .unwrap()
            .calls
            .push(("open_tab".into(), json!(url)));
        Ok("T3".into())
    }

    fn switch_tab(&mut self, id: &str) -> Result<(), ToolFailure> {
        self.0
            .lock()
            .unwrap()
            .calls
            .push(("switch_tab".into(), json!(id)));
        match id {
            "T1" | "T2" | "T3" => Ok(()),
            _ => Err(ToolFailure::TargetGone(format!("there is no tab {id}"))),
        }
    }

    fn close_tab(&mut self, id: &str) -> Result<(), ToolFailure> {
        self.0
            .lock()
            .unwrap()
            .calls
            .push(("close_tab".into(), json!(id)));
        Ok(())
    }

    fn close(&mut self) {
        self.0
            .lock()
            .unwrap()
            .calls
            .push(("close".into(), json!(null)));
    }
}

struct FakeOpener(Shared, AtomicUsize);

impl Opener for FakeOpener {
    fn open(&self, _display: &str) -> Result<Box<dyn Tools>, BrowserError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Fake(self.0.clone())))
    }
}

struct Harness {
    world: Shared,
    opener: Arc<FakeOpener>,
    browsers: Browsers,
    root: tempfile::TempDir,
}

fn harness() -> Harness {
    let world: Shared = Arc::default();
    {
        let mut w = world.lock().unwrap();
        w.url = "https://shop.example/cart".into();
        w.elements = vec![
            button(1, "n10", "Search"),
            json!({"index": 2, "region": "n20", "role": "textbox", "label": "Email",
                   "input_type": "email", "value": "kid@example.com", "actions": ["type"],
                   "rect": {"x": 0, "y": 60, "w": 200, "h": 30}}),
            json!({"index": 3, "region": "n30", "role": "textbox", "label": "Password",
                   "input_type": "password", "value": "hunter2", "actions": ["type"],
                   "rect": {"x": 0, "y": 100, "w": 200, "h": 30}}),
            button(4, "n40", "Pay $18"),
            button(5, "n50", "Delete account"),
        ];
    }
    let opener = Arc::new(FakeOpener(world.clone(), AtomicUsize::new(0)));
    let root = tempfile::tempdir().unwrap();
    let browsers = Browsers::new(
        true,
        Rules::default(),
        opener.clone(),
        root.path().to_path_buf(),
    );
    Harness {
        world,
        opener,
        browsers,
        root,
    }
}

fn act(reference: &str, action: ActAction, text: Option<&str>) -> ActRequest {
    ActRequest {
        reference: reference.into(),
        snapshot: None,
        action,
        text: text.map(str::to_string),
        confirm: None,
        allow_domains: None,
    }
}

fn calls_of(h: &Harness, name: &str) -> Vec<Value> {
    h.world
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, a)| a.clone())
        .collect()
}

#[tokio::test]
async fn a_snapshot_is_shaped_cut_and_numbered() {
    let h = harness();
    let snap = h
        .browsers
        .snap(
            ":1",
            SnapRequest {
                max_nodes: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(snap.elements.len(), 2);
    assert!(snap.truncated);
    assert_eq!(snap.total, 5);
    assert_eq!(snap.elements[0].reference, "@e1");
    assert_eq!(snap.elements[1].value, None);
    assert_eq!(snap.url.as_deref(), Some("https://shop.example/cart"));
    assert!(snap.text.starts_with("@e1 button \"Search\"\n"));
    let wire = serde_json::to_value(&snap).unwrap();
    // Nothing of ultra-instinct's own passes through: no region ids, no indexes.
    assert!(!wire.to_string().contains("n10"));
    assert!(wire["elements"][0].get("index").is_none());

    let next = h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    assert!(next.snapshot > snap.snapshot);
    assert!(!next.truncated);
    assert!(h
        .browsers
        .snap(
            ":1",
            SnapRequest {
                max_nodes: Some(0),
                ..Default::default()
            },
        )
        .await
        .is_err());
}

#[tokio::test]
async fn a_password_field_s_text_is_never_returned() {
    let h = harness();
    let snap = h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let wire = serde_json::to_string(&snap).unwrap();
    assert!(!wire.contains("hunter2"), "{wire}");
    let password = &snap.elements[2];
    assert!(password.sensitive);
    assert_eq!(password.value, None);
    assert!(snap.text.contains("@e3 textbox \"Password\" [sensitive]"));
}

fn with_values() -> SnapRequest {
    SnapRequest {
        values: true,
        allow_domains: Some(vec!["shop.example".into()]),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_snap_carries_no_values_unless_asked() {
    let h = harness();
    {
        let mut w = h.world.lock().unwrap();
        w.elements
            .push(json!({"index": 6, "region": "n60", "role": "checkbox",
            "label": "Gift wrap", "input_type": "checkbox", "checked": true,
            "actions": ["toggle"]}));
        w.elements.push(
            json!({"index": 7, "region": "n70", "role": "tab", "label": "Reviews",
            "selected_item": true, "aria_current": "page", "actions": ["click"]}),
        );
    }
    let snap = h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    assert_eq!(
        calls_of(&h, "browser_get_state")[0],
        json!({"format": "json"})
    );
    let wire = serde_json::to_string(&snap).unwrap();
    for absent in [
        "kid@example.com",
        "\"value",
        "checked",
        "expanded",
        "selected",
        "current",
    ] {
        assert!(!wire.contains(absent), "{absent}: {wire}");
    }
    assert!(
        snap.text.contains("@e2 textbox \"Email\"\n"),
        "{}",
        snap.text
    );
    // the body says so as well
    let req: SnapRequest = serde_json::from_value(json!({})).unwrap();
    assert!(!req.values);
    let req: SnapRequest = serde_json::from_value(json!({"values": true})).unwrap();
    assert!(req.limits().unwrap().values);
}

#[tokio::test]
async fn values_come_when_asked_and_never_for_a_password_or_a_sensitive_field() {
    let h = harness();
    let long = "z".repeat(shape::MAX_VALUE_CHARS + 50);
    {
        let mut w = h.world.lock().unwrap();
        w.elements.extend([
            json!({"index": 6, "region": "n60", "role": "checkbox", "label": "Gift wrap",
                   "input_type": "checkbox", "checked": true, "actions": ["toggle"]}),
            json!({"index": 7, "region": "n70", "role": "textbox", "label": "Card",
                   "input_type": "text", "sensitive": true, "value": "4111111111111111",
                   "checked": false, "expanded": false, "actions": ["type"]}),
            json!({"index": 8, "region": "n80", "role": "textbox", "label": "Code",
                   "input_type": "hidden", "value": "csrf-s3cret", "actions": ["type"]}),
            json!({"index": 9, "region": "n90", "role": "textbox", "label": "Note",
                   "input_type": "text", "value": long, "actions": ["type"]}),
            json!({"index": 10, "region": "n100", "role": "generic", "label": "Size",
                   "value": "Large", "actions": ["focus", "click", "select"]}),
            json!({"index": 11, "region": "n110", "role": "textbox", "label": "Untyped",
                   "value": "no type, no text", "actions": ["type"]}),
            json!({"index": 12, "region": "n120", "role": "tab", "label": "Reviews",
                   "selected_item": true, "actions": ["click"]}),
            json!({"index": 13, "region": "n130", "role": "link", "label": "Cart",
                   "selected_item": false, "aria_current": "page", "actions": ["click"]}),
        ]);
        // a sensitive field says nothing of these either
        w.elements[6]["selected_item"] = json!(true);
        w.elements[6]["aria_current"] = json!("step");
    }
    let snap = h.browsers.snap(":1", with_values()).await.unwrap();
    assert_eq!(
        calls_of(&h, "browser_get_state")[0],
        json!({"format": "json", "values": true})
    );
    let by = |label: &str| snap.elements.iter().find(|e| e.label == label).unwrap();
    assert_eq!(by("Email").value.as_deref(), Some("kid@example.com"));
    assert!(!by("Email").value_truncated);
    assert_eq!(by("Gift wrap").checked, Some(true));
    assert_eq!(by("Size").value.as_deref(), Some("Large"));
    assert_eq!(by("Untyped").value, None);
    let note = by("Note");
    assert_eq!(
        note.value.as_ref().unwrap().chars().count(),
        shape::MAX_VALUE_CHARS + 1
    );
    assert!(note.value_truncated);
    for secret in ["Password", "Card", "Code"] {
        let e = by(secret);
        assert_eq!(
            (e.value.as_deref(), e.checked, e.expanded, e.value_truncated),
            (None, None, None, false),
            "{secret}"
        );
        assert_eq!((e.selected, e.current.as_deref()), (None, None), "{secret}");
    }
    assert_eq!(
        (by("Reviews").selected, by("Reviews").current.as_deref()),
        (Some(true), None)
    );
    assert_eq!(
        (by("Cart").selected, by("Cart").current.as_deref()),
        (Some(false), Some("page"))
    );
    let wire = serde_json::to_string(&snap).unwrap();
    for secret in ["hunter2", "4111111111111111", "csrf-s3cret"] {
        assert!(!wire.contains(secret), "{secret}: {wire}");
    }
    assert!(wire.contains("\"value_truncated\":true"));
    assert!(snap
        .text
        .contains("@e2 textbox \"Email\" value=\"kid@example.com\"\n"));
    assert!(snap.text.contains("@e3 textbox \"Password\" [sensitive]\n"));
    assert!(snap.text.contains("@e6 checkbox \"Gift wrap\" checked\n"));
    assert!(
        snap.text.contains("@e12 tab \"Reviews\" selected\n"),
        "{}",
        snap.text
    );
    assert!(
        snap.text.contains("@e13 link \"Cart\" current=\"page\"\n"),
        "{}",
        snap.text
    );
    let reviews = &serde_json::to_value(&snap).unwrap()["elements"][11];
    assert_eq!(reviews["selected"], json!(true));
    assert!(reviews.get("current").is_none() && reviews.get("selected_item").is_none());

    // a press's own look of the page asks for nothing it holds
    h.browsers
        .act(":1", act("@e1", ActAction::Click, None))
        .await
        .unwrap();
    let looks = calls_of(&h, "browser_get_state");
    assert_eq!(looks.len(), 2);
    assert_eq!(looks[1], json!({"format": "json"}));
}

#[tokio::test]
async fn refs_are_good_for_the_latest_snapshot_alone() {
    let h = harness();
    let stale = h
        .browsers
        .act(":1", act("@e1", ActAction::Click, None))
        .await;
    assert!(matches!(stale, Err(BrowserError::Stale(_))), "{stale:?}");

    let first = h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let second = h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let mut old = act("@e1", ActAction::Click, None);
    old.snapshot = Some(first.snapshot);
    assert!(matches!(
        h.browsers.act(":1", old).await,
        Err(BrowserError::Stale(_))
    ));
    let long_form = act(&format!("{}:@e1", second.snapshot), ActAction::Click, None);
    assert!(h.browsers.act(":1", long_form).await.is_ok());
    assert!(matches!(
        h.browsers
            .act(":1", act("@e9", ActAction::Click, None))
            .await,
        Err(BrowserError::Invalid(_))
    ));

    // The element left the page: the ref is stale, and nothing was pressed.
    h.world
        .lock()
        .unwrap()
        .elements
        .retain(|e| e["region"] != "n10");
    let before = calls_of(&h, "browser_click").len();
    assert!(matches!(
        h.browsers
            .act(":1", act("@e1", ActAction::Click, None))
            .await,
        Err(BrowserError::Stale(_))
    ));
    assert_eq!(calls_of(&h, "browser_click").len(), before);

    // A navigation ends the snapshot.
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    h.browsers
        .navigate(
            ":1",
            NavigateRequest {
                url: "https://shop.example/".into(),
                allow_domains: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        h.browsers
            .act(":1", act("@e2", ActAction::Focus, None))
            .await,
        Err(BrowserError::Stale(_))
    ));
}

#[tokio::test]
async fn an_element_that_moved_is_found_where_it_went() {
    let h = harness();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    // Something new above Search: Search is now @e2 on the page, still @e1 in the snapshot.
    {
        let mut w = h.world.lock().unwrap();
        for e in w.elements.iter_mut() {
            let i = e["index"].as_u64().unwrap();
            e["index"] = json!(i + 1);
        }
        w.elements.insert(0, button(1, "n5", "Banner"));
    }
    let done = h
        .browsers
        .act(":1", act("@e1", ActAction::Click, None))
        .await
        .unwrap();
    assert_eq!(done.ticket, Some(41));
    assert_eq!(calls_of(&h, "browser_click")[0]["index"], 2);
}

/// A press that landed on another element than the ref named is no `stale_ref` ("nothing
/// happened"): something was pressed, and the caller hears so, with that element's label when the
/// look before the press showed it.
#[tokio::test]
async fn a_press_that_lands_elsewhere_says_something_was_pressed() {
    let h = harness();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    h.world.lock().unwrap().drift_to = Some("n50".into());
    let landed = h
        .browsers
        .act(":1", act("@e1", ActAction::Click, None))
        .await;
    let Err(err @ BrowserError::PressedOther { .. }) = landed else {
        panic!("{landed:?}");
    };
    assert_eq!(
        err,
        BrowserError::PressedOther {
            reference: "@e1".into(),
            label: Some("Delete account".into()),
        }
    );
    assert!(
        err.to_string().contains("snap before doing anything else"),
        "{err}"
    );
    assert_eq!(calls_of(&h, "browser_click").len(), 1);
    // its snapshot is over: the next act on it is stale until a snap
    assert!(matches!(
        h.browsers
            .act(":1", act("@e1", ActAction::Click, None))
            .await,
        Err(BrowserError::Stale(_))
    ));
    // one the look didn't show has no label to give
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    h.world.lock().unwrap().drift_to = Some("n99".into());
    assert!(matches!(
        h.browsers
            .act(":1", act("@e1", ActAction::Click, None))
            .await,
        Err(BrowserError::PressedOther { label: None, .. })
    ));
}

#[tokio::test]
async fn a_pay_click_needs_a_person_s_confirm() {
    let h = harness();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let Err(BrowserError::ConfirmRequired { label, token }) = h
        .browsers
        .act(":1", act("@e4", ActAction::Click, None))
        .await
    else {
        panic!("a Pay click went through");
    };
    assert_eq!(label, "Pay $18");
    assert!(calls_of(&h, "browser_click").is_empty());

    // Typing into a Pay button isn't a click: no confirm. A wrong token is no confirm.
    let mut wrong = act("@e4", ActAction::Click, None);
    wrong.confirm = Some("0123".into());
    assert!(matches!(
        h.browsers.act(":1", wrong).await,
        Err(BrowserError::ConfirmRequired { .. })
    ));

    let mut confirmed = act("@e4", ActAction::Click, None);
    confirmed.confirm = Some(token.clone());
    let done = h.browsers.act(":1", confirmed).await.unwrap();
    assert!(done.ok);
    let clicks = calls_of(&h, "browser_click");
    assert_eq!(clicks.len(), 1);
    assert_eq!(clicks[0]["confirm"], "Pay $18");
    let policies = h.world.lock().unwrap().policies.clone();
    assert!(policies
        .last()
        .unwrap()
        .confirmed
        .contains(&"Pay $18".to_string()));

    // Spent: the same token again is a new confirm.
    let mut again = act("@e4", ActAction::Click, None);
    again.confirm = Some(token);
    assert!(matches!(
        h.browsers.act(":1", again).await,
        Err(BrowserError::ConfirmRequired { .. })
    ));
    // And the next plain click on Search runs under a policy with nothing confirmed.
    h.browsers
        .act(":1", act("@e1", ActAction::Click, None))
        .await
        .unwrap();
    assert!(h
        .world
        .lock()
        .unwrap()
        .policies
        .last()
        .unwrap()
        .confirmed
        .is_empty());
}

#[tokio::test]
async fn ultra_instinct_s_own_confirm_is_answered_the_same_way() {
    let h = harness();
    // Its guard holds a glob box-browser's rules don't.
    h.world.lock().unwrap().extra_confirm = vec!["Search".into()];
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let Err(BrowserError::ConfirmRequired { label, token }) = h
        .browsers
        .act(":1", act("@e1", ActAction::Click, None))
        .await
    else {
        panic!("the guard's refusal was lost");
    };
    assert_eq!(label, "Search");
    assert_eq!(token.len(), 32);
}

#[tokio::test]
async fn off_the_allow_list_there_is_no_input_and_no_navigation() {
    let h = harness();
    h.world.lock().unwrap().url = "https://evil.example/".into();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let mut typed = act("@e2", ActAction::Type, Some("kid@example.com"));
    typed.allow_domains = Some(vec!["shop.example".into()]);
    let refused = h.browsers.act(":1", typed).await;
    assert!(
        matches!(&refused, Err(BrowserError::DomainNotAllowed(host)) if host == "evil.example"),
        "{refused:?}"
    );
    assert!(calls_of(&h, "browser_type").is_empty());

    let go = |url: &str| NavigateRequest {
        url: url.into(),
        allow_domains: Some(vec!["*.shop.example".into(), "shop.example".into()]),
    };
    assert!(matches!(
        h.browsers
            .navigate(":1", go("https://evil.example/x"))
            .await,
        Err(BrowserError::DomainNotAllowed(_))
    ));
    assert!(calls_of(&h, "browser_navigate").is_empty());
    h.browsers
        .navigate(":1", go("https://www.shop.example/"))
        .await
        .unwrap();
    let policy = h.world.lock().unwrap().policies.last().cloned().unwrap();
    assert_eq!(policy.allow_domains, vec!["*.shop.example", "shop.example"]);
    assert_eq!(policy.confirm_press, Rules::default().globs());

    let open = TabsRequest {
        op: TabsOp::Open,
        tab_id: None,
        url: Some("https://evil.example/".into()),
        allow_domains: Some(vec!["shop.example".into()]),
    };
    assert!(matches!(
        h.browsers.tabs(":1", open).await,
        Err(BrowserError::DomainNotAllowed(_))
    ));
    assert!(matches!(
        h.browsers
            .navigate(
                ":1",
                NavigateRequest {
                    url: "file:///etc/passwd".into(),
                    allow_domains: None
                }
            )
            .await,
        Err(BrowserError::Invalid(_))
    ));
}

#[tokio::test]
async fn acts_take_their_text_and_nothing_else() {
    let h = harness();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    assert!(h
        .browsers
        .act(":1", act("@e2", ActAction::Type, None))
        .await
        .is_err());
    assert!(h
        .browsers
        .act(":1", act("@e1", ActAction::Click, Some("x")))
        .await
        .is_err());
    h.browsers
        .act(":1", act("@e2", ActAction::Type, Some("kid@example.com")))
        .await
        .unwrap();
    h.browsers
        .act(":1", act("@e2", ActAction::Key, Some("Tab")))
        .await
        .unwrap();
    h.browsers
        .act(":1", act("@e2", ActAction::Scroll, None))
        .await
        .unwrap();
    assert_eq!(calls_of(&h, "browser_type")[0]["text"], "kid@example.com");
    assert_eq!(calls_of(&h, "browser_send_key")[0]["key"], "Tab");
    assert_eq!(calls_of(&h, "browser_scroll_to")[0]["index"], 2);
}

#[tokio::test]
async fn goal_runs_end_done_blocked_abstained_or_failed() {
    let h = harness();
    let run = |goal: &str| RunRequest {
        goal: goal.into(),
        max_steps: Some(7),
        timeout_ms: None,
        allow_domains: None,
        confirm: None,
    };
    let step = GoalStep {
        action: "type".into(),
        target: "Search".into(),
        effect: "success".into(),
    };
    h.world.lock().unwrap().goal = Some(GoalReport {
        end: GoalEnd::Done,
        steps: vec![step.clone()],
    });
    let done = h
        .browsers
        .run_goal(":1", run("Type \"rust\" into Search"))
        .await
        .unwrap();
    assert_eq!(done.outcome, Outcome::Done);
    assert_eq!(done.steps[0].target, "Search");
    assert_eq!(h.world.lock().unwrap().goals[0].1, 7);

    h.world.lock().unwrap().goal = Some(GoalReport {
        end: GoalEnd::Blocked {
            reason: "domain-not-allowed".into(),
            by: BlockedBy::Domain {
                host: Some("partner.example".into()),
            },
        },
        steps: vec![],
    });
    // off the list: blocked, and no person is asked, since a confirm can't help
    let off_list = h
        .browsers
        .run_goal(":1", run("Go to the partner"))
        .await
        .unwrap();
    assert_eq!(off_list.outcome, Outcome::Blocked);
    assert!(off_list.confirm.is_none() && off_list.confirm_label.is_none());
    assert_eq!(off_list.code, Some("domain_not_allowed"));
    assert_eq!(off_list.host.as_deref(), Some("partner.example"));

    // a run blocked on a page with no host: ultra-instinct names its scheme, and that comes through
    h.world.lock().unwrap().goal = Some(GoalReport {
        end: GoalEnd::Blocked {
            reason: "domain-not-allowed".into(),
            by: BlockedBy::Domain {
                host: Some("a blob: page".into()),
            },
        },
        steps: vec![],
    });
    let on_blob = h
        .browsers
        .run_goal(":1", run("Open the receipt"))
        .await
        .unwrap();
    assert_eq!(on_blob.code, Some("domain_not_allowed"));
    assert_eq!(on_blob.host.as_deref(), Some("a blob: page"));

    // the guard's own label for a form it can't see the button of: still a person's to confirm
    h.world.lock().unwrap().goal = Some(GoalReport {
        end: GoalEnd::Blocked {
            reason: "needs-confirm".into(),
            by: BlockedBy::Confirm {
                label: "(form submit)".into(),
            },
        },
        steps: vec![],
    });
    let unseen = h.browsers.run_goal(":1", run("Sign up")).await.unwrap();
    assert_eq!(unseen.confirm_label.as_deref(), Some("(form submit)"));
    assert!(unseen.confirm.is_some());

    // the run stopped itself: no code, no token
    h.world.lock().unwrap().goal = Some(GoalReport {
        end: GoalEnd::Blocked {
            reason: "loop".into(),
            by: BlockedBy::Loop,
        },
        steps: vec![],
    });
    let looped = h.browsers.run_goal(":1", run("Go round")).await.unwrap();
    assert!(looped.code.is_none() && looped.confirm.is_none());

    h.world.lock().unwrap().goal = Some(GoalReport {
        end: GoalEnd::Blocked {
            reason: "needs-confirm".into(),
            by: BlockedBy::Confirm {
                label: "Place order".into(),
            },
        },
        steps: vec![step.clone()],
    });
    let blocked = h.browsers.run_goal(":1", run("Buy it")).await.unwrap();
    assert_eq!(blocked.outcome, Outcome::Blocked);
    assert_eq!(blocked.confirm_label.as_deref(), Some("Place order"));
    assert_eq!(blocked.code, Some("needs_confirm"));
    let token = blocked.confirm.clone().unwrap();
    let wire = serde_json::to_value(&blocked).unwrap();
    assert_eq!(wire["outcome"], "blocked");

    h.world.lock().unwrap().goal = Some(GoalReport {
        end: GoalEnd::Abstained {
            why: "two Buy buttons".into(),
        },
        steps: vec![],
    });
    let mut confirmed = run("Buy it");
    confirmed.confirm = Some(token.clone());
    let abstained = h.browsers.run_goal(":1", confirmed).await.unwrap();
    assert_eq!(abstained.outcome, Outcome::Abstained);
    assert_eq!(abstained.why.as_deref(), Some("two Buy buttons"));
    let policies = h.world.lock().unwrap().policies.clone();
    assert!(policies
        .iter()
        .any(|p| p.confirmed == vec!["Place order".to_string()]));
    let mut reused = run("Buy it");
    reused.confirm = Some(token);
    assert!(matches!(
        h.browsers.run_goal(":1", reused).await,
        Err(BrowserError::Invalid(_))
    ));

    h.world.lock().unwrap().goal = Some(GoalReport {
        end: GoalEnd::Failed {
            why: "bounds".into(),
        },
        steps: vec![],
    });
    let failed = h.browsers.run_goal(":1", run("x")).await.unwrap();
    assert_eq!(failed.outcome, Outcome::Failed);

    let mut too_many = run("x");
    too_many.max_steps = Some(MAX_STEPS + 1);
    assert!(h.browsers.run_goal(":1", too_many).await.is_err());
    let mut too_long = run("x");
    too_long.timeout_ms = Some(MAX_RUN_MS + 1);
    assert!(h.browsers.run_goal(":1", too_long).await.is_err());
}

/// A run under a list starting on a `blob:` page is refused by box-browser before ultra-instinct is
/// asked, as an act on one is. One the run reaches on its own is refused by ultra-instinct's guard,
/// under the policy box-browser gives it (`seam`'s test).
#[tokio::test]
async fn a_goal_run_on_a_blob_page_is_refused_under_a_list() {
    let h = harness();
    let run = |url: &str| {
        h.world.lock().unwrap().url = url.into();
        RunRequest {
            goal: "Click Search".into(),
            max_steps: None,
            timeout_ms: None,
            allow_domains: Some(vec!["shop.example".into()]),
            confirm: None,
        }
    };
    h.world.lock().unwrap().goal = Some(GoalReport {
        end: GoalEnd::Done,
        steps: vec![],
    });
    let blob = run("blob:https://shop.example/0b7c6a3e");
    assert!(matches!(
        h.browsers.run_goal(":1", blob).await,
        Err(BrowserError::DomainNotAllowed(_))
    ));
    assert!(h.world.lock().unwrap().goals.is_empty());
    let page = run("https://shop.example/cart");
    let done = h.browsers.run_goal(":1", page).await.unwrap();
    assert_eq!(done.outcome, Outcome::Done);
    assert_eq!(h.world.lock().unwrap().goals.len(), 1);
}

#[tokio::test]
async fn a_saved_login_is_a_0600_file_in_the_screen_s_own_0700_folder() {
    let h = harness();
    let save = StateRequest {
        op: StateOp::Save,
        name: None,
    };
    let saved = h.browsers.state(":1", "main", save).await.unwrap();
    assert_eq!(saved.cookies, 1);
    assert_eq!(saved.origins, vec!["https://shop.example"]);
    assert!(!serde_json::to_string(&saved).unwrap().contains("s3cret"));
    let folder = h.root.path().join("main");
    let file = folder.join("default.json");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&folder), 0o700);
    assert_eq!(mode(&file), 0o600);
    assert_eq!(calls_of(&h, "browser_state_save")[0]["path"], json!(file));

    let load = || StateRequest {
        op: StateOp::Load,
        name: Some("default".into()),
    };
    let loaded = h.browsers.state(":1", "main", load()).await.unwrap();
    assert_eq!(loaded.pending_origins, vec!["https://other.example"]);

    // One others can read is refused, and so is a name that isn't one, or a login never saved.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(h.browsers.state(":1", "main", load()).await.is_err());
    let bad = StateRequest {
        op: StateOp::Save,
        name: Some("../../etc/x".into()),
    };
    assert!(h.browsers.state(":1", "main", bad).await.is_err());
    assert!(h.browsers.state(":1", "../x", load()).await.is_err());
    let never = StateRequest {
        op: StateOp::Load,
        name: Some("nope".into()),
    };
    assert!(h.browsers.state(":2", "bot-a", never).await.is_err());
}

#[tokio::test]
async fn a_shot_is_a_png_within_its_cap() {
    let h = harness();
    let mut png = vec![
        0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0, 0, 13,
    ];
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&1280u32.to_be_bytes());
    png.extend_from_slice(&800u32.to_be_bytes());
    h.world.lock().unwrap().shot = png.clone();
    let shot = h.browsers.shot(":1").await.unwrap();
    assert_eq!(
        (shot.width, shot.height, shot.bytes),
        (1280, 800, png.len())
    );
    h.world.lock().unwrap().shot = b"not a png".to_vec();
    assert!(matches!(
        h.browsers.shot(":1").await,
        Err(BrowserError::Backend(_))
    ));
    h.world.lock().unwrap().shot = vec![0u8; screen::MAX_SHOT_BYTES + 1];
    assert!(matches!(
        h.browsers.shot(":1").await,
        Err(BrowserError::Backend(_))
    ));
}

#[tokio::test]
async fn tabs_and_waits_come_back_in_box_browser_s_shapes() {
    let h = harness();
    let list = TabsRequest {
        op: TabsOp::List,
        tab_id: None,
        url: None,
        allow_domains: None,
    };
    let tabs = h.browsers.tabs(":1", list).await.unwrap();
    assert_eq!(tabs.tabs.unwrap().len(), 2);
    // Tabs never go through ultra-instinct's HTTP-backed tab tools.
    assert!(calls_of(&h, "browser_list_tabs").is_empty());
    let gone = TabsRequest {
        op: TabsOp::Switch,
        tab_id: Some("T9".into()),
        url: None,
        allow_domains: None,
    };
    assert!(matches!(
        h.browsers.tabs(":1", gone).await,
        Err(BrowserError::TargetGone(_))
    ));
    let switch = TabsRequest {
        op: TabsOp::Switch,
        tab_id: Some("T2".into()),
        url: None,
        allow_domains: None,
    };
    assert_eq!(
        h.browsers
            .tabs(":1", switch)
            .await
            .unwrap()
            .tab_id
            .as_deref(),
        Some("T2")
    );
    let close = TabsRequest {
        op: TabsOp::Close,
        tab_id: None,
        url: None,
        allow_domains: None,
    };
    assert!(h.browsers.tabs(":1", close).await.is_err());

    let wait = WaitRequest {
        text: Some("Welcome".into()),
        timeout_ms: Some(2000),
        ..Default::default()
    };
    let seen = h.browsers.wait(":1", wait).await.unwrap();
    assert!(seen.seen);
    assert_eq!(calls_of(&h, "browser_wait")[0]["seconds"], 2.0);
    let too_long = WaitRequest {
        timeout_ms: Some(MAX_WAIT_MS + 1),
        ..Default::default()
    };
    assert!(h.browsers.wait(":1", too_long).await.is_err());
}

#[tokio::test]
async fn a_closed_pipe_opens_again_on_the_next_call() {
    let h = harness();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    h.world.lock().unwrap().fail_next = Some(ToolFailure::Closed("pipe closed".into()));
    assert!(matches!(
        h.browsers.snap(":1", SnapRequest::default()).await,
        Err(BrowserError::Unavailable(_))
    ));
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    assert_eq!(h.opener.1.load(Ordering::SeqCst), 2);
    // Another screen has a thread and tools of its own.
    h.browsers.snap(":2", SnapRequest::default()).await.unwrap();
    assert_eq!(h.opener.1.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn shutting_down_asks_each_browser_to_quit_and_lets_go() {
    let h = harness();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    h.browsers.snap(":2", SnapRequest::default()).await.unwrap();
    h.browsers
        .close_all(std::time::Duration::from_secs(2))
        .await;
    assert_eq!(calls_of(&h, "close").len(), 2);
    // A call after that opens the screen again.
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    assert_eq!(h.opener.1.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_confirm_covers_its_press_and_nothing_after() {
    let h = harness();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let Err(BrowserError::ConfirmRequired { token, .. }) = h
        .browsers
        .act(":1", act("@e4", ActAction::Click, None))
        .await
    else {
        panic!("no confirm asked");
    };
    let mut confirmed = act("@e4", ActAction::Click, None);
    confirmed.confirm = Some(token);
    h.browsers.act(":1", confirmed).await.unwrap();
    // The very next press on the same label, with no token, is refused again before it's sent.
    let before = calls_of(&h, "browser_click").len();
    assert!(matches!(
        h.browsers
            .act(":1", act("@e4", ActAction::Click, None))
            .await,
        Err(BrowserError::ConfirmRequired { .. })
    ));
    assert_eq!(calls_of(&h, "browser_click").len(), before);
    // And the policy the guard holds for the next press has nothing confirmed.
    h.browsers
        .act(":1", act("@e1", ActAction::Click, None))
        .await
        .unwrap();
    assert!(h
        .world
        .lock()
        .unwrap()
        .policies
        .last()
        .unwrap()
        .confirmed
        .is_empty());
}

#[tokio::test]
async fn a_call_that_panics_keeps_the_screen_s_pipe() {
    let h = harness();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    h.world.lock().unwrap().panic_next = true;
    assert!(h.browsers.snap(":1", SnapRequest::default()).await.is_err());
    // The snapshot went; the tools, and so the pipe, did not.
    assert!(matches!(
        h.browsers
            .act(":1", act("@e1", ActAction::Click, None))
            .await,
        Err(BrowserError::Stale(_))
    ));
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    assert_eq!(h.opener.1.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_off_layer_answers_nothing() {
    let off = Browsers::disabled();
    assert!(matches!(
        off.snap(":1", SnapRequest::default()).await,
        Err(BrowserError::Disabled)
    ));
}

/// ultra-instinct's real `Server` on a replayed page: box-browser's snap reads what it gives.
#[tokio::test]
async fn ultra_instinct_s_own_answer_is_read() {
    use ultra_instinct_cdp::script::{Control, PageSpec, ScriptBuilder};
    use ultra_instinct_cdp::{CdpTransport, ReplayTransport};

    struct Replay;
    impl Opener for Replay {
        fn open(&self, _display: &str) -> Result<Box<dyn Tools>, BrowserError> {
            let page = PageSpec::of(
                &[
                    Control::button(10, 100, "Sign in", (400.0, 300.0, 80.0, 32.0)),
                    Control::text_field(20, 200, "Email", (400.0, 200.0, 200.0, 32.0)),
                ],
                "https://shop.example/login",
                "Sign in",
            );
            let script = ScriptBuilder::new().observe(&page).to_json();
            let server = ultra_instinct_mcp::Server::with_connector(move |_url| {
                ReplayTransport::parse(&script).map(|t| Box::new(t) as Box<dyn CdpTransport>)
            });
            Ok(Box::new(seam::ServerTools::new(server)))
        }
    }
    let browsers = Browsers::new(
        true,
        Rules::default(),
        Arc::new(Replay),
        std::env::temp_dir(),
    );
    let snap = browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let labels: Vec<_> = snap.elements.iter().map(|e| e.label.as_str()).collect();
    assert!(labels.contains(&"Sign in"), "{snap:?}");
    assert!(labels.contains(&"Email"), "{snap:?}");
    assert_eq!(snap.url.as_deref(), Some("https://shop.example/login"));
    assert!(snap.elements.iter().all(|e| e.reference.starts_with("@e")));

    // ultra-instinct takes `values` next to `format: json`; the replayed field's type can't be
    // read, so it's sensitive (failing closed) and holds nothing to show
    let browsers = Browsers::new(
        true,
        Rules::default(),
        Arc::new(Replay),
        std::env::temp_dir(),
    );
    let snap = browsers.snap(":1", with_values()).await.unwrap();
    assert_eq!(snap.elements.len(), 2, "{snap:?}");
    let email = snap.elements.iter().find(|e| e.label == "Email").unwrap();
    assert!(email.sensitive);
    assert_eq!(email.value, None);
}

/// ultra-instinct's real `Server` with `values: true`, on a replayed page whose fields hold text:
/// what box-browser shows of each.
#[tokio::test]
async fn ultra_instinct_s_values_are_read_and_held_back() {
    use ultra_instinct_cdp::script::{Control, PageSpec, ScriptBuilder};
    use ultra_instinct_cdp::{CdpTransport, ReplayTransport};

    struct Replay;
    impl Opener for Replay {
        fn open(&self, _display: &str) -> Result<Box<dyn Tools>, BrowserError> {
            let select = Control {
                tag: "SELECT",
                role: "combobox",
                ..Control::combobox(13, 103, "Country", (20.0, 140.0, 200.0, 28.0))
            };
            let checkbox = Control {
                tag: "INPUT",
                role: "checkbox",
                ..Control::button(14, 104, "Send me news", (20.0, 180.0, 20.0, 20.0))
            };
            let mut page = PageSpec::of(
                &[
                    Control::text_field(10, 100, "Email", (20.0, 20.0, 300.0, 28.0)),
                    Control::text_field(11, 101, "Password", (20.0, 60.0, 300.0, 28.0)),
                    Control::text_field(12, 102, "Card", (20.0, 100.0, 300.0, 28.0)),
                    select,
                    checkbox,
                    Control::link(15, 105, "Reviews", (20.0, 220.0, 80.0, 20.0)),
                ],
                "https://shop.example/join",
                "Join",
            );
            let states = [
                json!({"it": "email", "v": "kid@example.com"}),
                json!({"it": "password", "v": "hunter2"}),
                json!({"it": "text", "v": "4111111111111111"}),
                json!({"o": ["Canada", "France"], "sel": "Canada"}),
                json!({"it": "checkbox", "c": true}),
                json!({"as": true, "ac": "page"}),
            ];
            for (node, state) in page.dom.iter_mut().zip(states) {
                *node = node.clone().with_compact_state(state);
            }
            page.dom[1] = page.dom[1].clone().with_attr("type", "password");
            page.dom[4] = page.dom[4].clone().with_attr("type", "checkbox");
            let script = ScriptBuilder::new().observe_compact(&page).to_json();
            let server = ultra_instinct_mcp::Server::with_connector(move |_url| {
                ReplayTransport::parse(&script).map(|t| Box::new(t) as Box<dyn CdpTransport>)
            });
            Ok(Box::new(seam::ServerTools::new(server)))
        }
    }
    let browsers = Browsers::new(
        true,
        Rules::default(),
        Arc::new(Replay),
        std::env::temp_dir(),
    );
    let snap = browsers.snap(":1", with_values()).await.unwrap();
    let by = |label: &str| {
        snap.elements
            .iter()
            .find(|e| e.label == label)
            .unwrap_or_else(|| panic!("no {label}: {snap:?}"))
    };
    assert_eq!(
        by("Email").value.as_deref(),
        Some("kid@example.com"),
        "{snap:?}"
    );
    // At this pin ultra-instinct reads a native select as a combobox, which takes typed text and
    // has no input type or text value: it fails closed, sensitive, and sends no option. Were it
    // to send one as a combobox, `may_return_selected` still holds it back: a combobox may be
    // a box a person typed in.
    let country = by("Country");
    assert!(country.sensitive, "{snap:?}");
    assert_eq!(country.value, None);
    assert_eq!(by("Send me news").checked, Some(true), "{snap:?}");
    let reviews = by("Reviews");
    assert!(!reviews.sensitive, "{snap:?}");
    assert_eq!(
        (reviews.selected, reviews.current.as_deref()),
        (Some(true), Some("page")),
        "{snap:?}"
    );
    assert!(
        snap.text
            .contains("\"Reviews\" selected current=\"page\"\n"),
        "{}",
        snap.text
    );
    for secret in ["Password", "Card"] {
        let e = by(secret);
        assert!(e.sensitive, "{secret}: {snap:?}");
        assert_eq!((e.value.as_deref(), e.checked), (None, None), "{secret}");
        assert_eq!((e.selected, e.current.as_deref()), (None, None), "{secret}");
    }
    let wire = serde_json::to_string(&snap).unwrap();
    assert!(
        !wire.contains("hunter2") && !wire.contains("4111111111111111"),
        "{wire}"
    );
}

/// ultra-instinct's real `Server`, built as `seam::open` builds it, on a replayed page: a click is
/// a real pointer press (move, press, release) at the element's centre, bound to the snapshot,
/// and never a DOM script click.
#[tokio::test]
async fn ultra_instinct_s_click_is_a_pointer_press_at_the_centre() {
    use ultra_instinct_cdp::script::{Control, PageSpec, ScriptBuilder};
    use ultra_instinct_cdp::{CdpError, CdpTransport, ReplayTransport};

    type Log = Arc<Mutex<Vec<(String, Value)>>>;
    struct Logged {
        inner: ReplayTransport,
        log: Log,
    }
    impl CdpTransport for Logged {
        fn call(&mut self, method: &str, params: &str) -> Result<String, CdpError> {
            let parsed = serde_json::from_str(params).unwrap_or(Value::Null);
            self.log.lock().unwrap().push((method.to_string(), parsed));
            self.inner.call(method, params)
        }
    }

    struct Replay(Log);
    impl Opener for Replay {
        fn open(&self, _display: &str) -> Result<Box<dyn Tools>, BrowserError> {
            let page = PageSpec::of(
                &[
                    Control::text_field(20, 200, "Email", (400.0, 200.0, 200.0, 32.0)),
                    Control::button(10, 100, "Sign in", (400.0, 300.0, 80.0, 32.0)),
                ],
                "https://shop.example/login",
                "Sign in",
            );
            // the snap, act's own look, then the press's two looks (its gate and its ticket)
            let script = ScriptBuilder::new()
                .observe_compact(&page)
                .observe_compact(&page)
                .observe_compact(&page)
                .observe_compact(&page)
                .pointer_click()
                .to_json();
            let log = Arc::clone(&self.0);
            let server = ultra_instinct_mcp::Server::with_connector(move |_url| {
                Ok(Box::new(Logged {
                    inner: ReplayTransport::parse(&script)?,
                    log: Arc::clone(&log),
                }) as Box<dyn CdpTransport>)
            })
            .require_snapshot(true)
            .with_tool_allowlist(seam::ALLOWED);
            Ok(Box::new(seam::ServerTools::new(server)))
        }
    }
    let log: Log = Arc::default();
    let browsers = Browsers::new(
        true,
        Rules::default(),
        Arc::new(Replay(Arc::clone(&log))),
        std::env::temp_dir(),
    );
    let snap = browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let sign_in = snap.elements.iter().find(|e| e.label == "Sign in").unwrap();
    browsers
        .act(":1", act(&sign_in.reference, ActAction::Click, None))
        .await
        .unwrap();
    let log = log.lock().unwrap();
    let presses: Vec<_> = log
        .iter()
        .filter(|(m, _)| {
            matches!(
                m.as_str(),
                "DOM.resolveNode" | "Runtime.callFunctionOn" | "Input.dispatchMouseEvent"
            )
        })
        .collect();
    let kinds: Vec<_> = presses
        .iter()
        .map(|(m, p)| format!("{m} {}", p["type"].as_str().unwrap_or_default()))
        .collect();
    assert_eq!(
        kinds,
        [
            "Input.dispatchMouseEvent mouseMoved",
            "Input.dispatchMouseEvent mousePressed",
            "Input.dispatchMouseEvent mouseReleased"
        ],
        "{log:?}"
    );
    // Sign in is 80x32 at (400, 300): its centre
    for (_, params) in presses {
        assert_eq!(
            (params["x"].as_f64(), params["y"].as_f64()),
            (Some(440.0), Some(316.0))
        );
    }
}

#[test]
fn a_jpegs_size_is_read_from_its_frame_header() {
    // SOI, an APP0 of 16 bytes, then a baseline SOF0 of 640x480
    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    jpeg.extend_from_slice(&[0; 14]);
    jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0xE0, 0x02, 0x80, 0x03]);
    jpeg.extend_from_slice(&[0; 9]);
    assert_eq!(crate::shape::jpeg_size(&jpeg), Some((640, 480)));
    // a scan before any frame header, a PNG, and a cut-off file are not sizes
    assert_eq!(
        crate::shape::jpeg_size(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02]),
        None
    );
    assert_eq!(crate::shape::jpeg_size(b"\x89PNG\r\n\x1a\n"), None);
    assert_eq!(crate::shape::jpeg_size(&jpeg[..24]), None);
}

#[tokio::test]
async fn a_key_that_can_press_a_pay_button_needs_its_confirm_too() {
    let h = harness();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    // Enter on the Pay button itself
    let Err(BrowserError::ConfirmRequired { label, .. }) = h
        .browsers
        .act(":1", act("@e4", ActAction::Key, Some("Enter")))
        .await
    else {
        panic!("Enter on Pay went through");
    };
    assert_eq!(label, "Pay $18");
    assert!(calls_of(&h, "browser_send_key").is_empty());
    // Enter or Space in a field: the guard knows the form it would send, and asks for its button
    for key in ["Enter", "Shift+Enter", "Space"] {
        let Err(BrowserError::ConfirmRequired { label, .. }) = h
            .browsers
            .act(":1", act("@e2", ActAction::Key, Some(key)))
            .await
        else {
            panic!("{key} in the field went through");
        };
        assert_eq!(label, "Pay $18", "{key}");
    }
    // a line typed while the page shows a Pay button may submit the form too: box-browser stops it
    assert!(matches!(
        h.browsers
            .act(":1", act("@e2", ActAction::Type, Some("kid@example.com\n")))
            .await,
        Err(BrowserError::ConfirmRequired { .. })
    ));
    assert!(calls_of(&h, "browser_type").is_empty());
    // the guard's token, once a person approves, lets that one Enter through
    let Err(BrowserError::ConfirmRequired { token, .. }) = h
        .browsers
        .act(":1", act("@e2", ActAction::Key, Some("Enter")))
        .await
    else {
        panic!("Enter in the field went through");
    };
    let mut approved = act("@e2", ActAction::Key, Some("Enter"));
    approved.confirm = Some(token.clone());
    h.browsers.act(":1", approved.clone()).await.unwrap();
    let sent = calls_of(&h, "browser_send_key");
    assert_eq!(sent.last().unwrap()["confirm"], "Pay $18");
    // spent: the same token again is no token
    assert!(matches!(
        h.browsers.act(":1", approved).await,
        Err(BrowserError::Invalid(_))
    ));
    // a key that submits nothing, and text with no line end, go through
    h.browsers
        .act(":1", act("@e2", ActAction::Key, Some("Tab")))
        .await
        .unwrap();
    h.browsers
        .act(":1", act("@e2", ActAction::Type, Some("kid@example.com")))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_goal_runs_confirm_is_for_that_goal_alone() {
    let h = harness();
    let run = |goal: &str| RunRequest {
        goal: goal.into(),
        max_steps: Some(7),
        timeout_ms: None,
        allow_domains: None,
        confirm: None,
    };
    h.world.lock().unwrap().goal = Some(GoalReport {
        end: GoalEnd::Blocked {
            reason: "needs-confirm".into(),
            by: BlockedBy::Confirm {
                label: "Pay $18".into(),
            },
        },
        steps: vec![],
    });
    let blocked = h
        .browsers
        .run_goal(":1", run("Pay for the cart"))
        .await
        .unwrap();
    let token = blocked.confirm.unwrap();
    // another goal can't spend it
    let mut other = run("Pay for everything");
    other.confirm = Some(token.clone());
    assert!(matches!(
        h.browsers.run_goal(":1", other).await,
        Err(BrowserError::Invalid(_))
    ));
    let mut same = run("Pay for the cart");
    same.confirm = Some(token);
    h.browsers.run_goal(":1", same).await.unwrap();
}

#[tokio::test]
async fn a_saved_login_runs_under_no_list_another_call_left() {
    let h = harness();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let mut press = act("@e1", ActAction::Click, None);
    press.allow_domains = Some(vec!["shop.example".into()]);
    h.browsers.act(":1", press).await.unwrap();
    let save = StateRequest {
        op: StateOp::Save,
        name: Some("login".into()),
    };
    h.browsers.state(":1", "main", save).await.unwrap();
    let last = h.world.lock().unwrap().policies.last().cloned().unwrap();
    assert!(last.allow_domains.is_empty(), "{last:?}");
}

#[tokio::test]
async fn a_ref_and_a_body_naming_two_snapshots_is_refused() {
    let h = harness();
    let first = h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let latest = h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    // the ref names the old snapshot, the body the latest: the old one must not be pressed as new
    let mut press = act(&format!("{}:@e1", first.snapshot), ActAction::Click, None);
    press.snapshot = Some(latest.snapshot);
    assert!(matches!(
        h.browsers.act(":1", press).await,
        Err(BrowserError::Invalid(_))
    ));
    assert!(calls_of(&h, "browser_click").is_empty());
}

#[tokio::test]
async fn an_approval_outlives_a_call_that_failed_before_its_press() {
    let h = harness();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let Err(BrowserError::ConfirmRequired { token, .. }) = h
        .browsers
        .act(":1", act("@e4", ActAction::Click, None))
        .await
    else {
        panic!("Pay went through");
    };
    let mut approved = act("@e4", ActAction::Click, None);
    approved.confirm = Some(token);
    // the policy can't be set: nothing is pressed, and the token is still good
    h.world.lock().unwrap().fail_policy = Some(ToolFailure::Browser("socket".into()));
    assert!(h.browsers.act(":1", approved.clone()).await.is_err());
    assert!(calls_of(&h, "browser_click").is_empty());
    h.browsers.act(":1", approved.clone()).await.unwrap();
    assert_eq!(calls_of(&h, "browser_click").len(), 1);
    // and once the press went out, it's spent
    assert!(matches!(
        h.browsers.act(":1", approved).await,
        Err(BrowserError::ConfirmRequired { .. })
    ));
}

#[test]
fn no_press_goes_out_without_time_to_answer() {
    let h = harness();
    let mut screen =
        crate::screen::Screen::new(":1".into(), h.opener.clone(), Arc::new(Rules::default()));
    let allow = crate::policy::AllowList::parse(None).unwrap();
    screen
        .snap(&SnapRequest::default().limits().unwrap(), &allow)
        .unwrap();
    screen.set_deadline(Some(std::time::Instant::now() + Duration::from_secs(1)));
    assert!(matches!(
        screen.act(&act("@e1", ActAction::Click, None), &allow),
        Err(BrowserError::Timeout(_))
    ));
    assert!(calls_of(&h, "browser_click").is_empty());
    screen.set_deadline(None);
    screen
        .act(&act("@e1", ActAction::Click, None), &allow)
        .unwrap();
}

#[tokio::test]
async fn a_long_label_is_confirmed_whole() {
    let h = harness();
    let long = format!("Pay {}", "now and forever ".repeat(20));
    h.world.lock().unwrap().elements[3] = button(4, "n40", &long);
    let snap = h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    assert!(snap.elements[3].label.chars().count() <= crate::shape::MAX_LABEL_CHARS + 1);
    let Err(BrowserError::ConfirmRequired { label, token }) = h
        .browsers
        .act(":1", act("@e4", ActAction::Click, None))
        .await
    else {
        panic!("Pay went through");
    };
    assert!(label.chars().count() <= crate::shape::MAX_LABEL_CHARS + 1);
    let mut approved = act("@e4", ActAction::Click, None);
    approved.confirm = Some(token);
    h.browsers.act(":1", approved).await.unwrap();
    assert_eq!(calls_of(&h, "browser_click")[0]["confirm"], json!(long));
}

#[tokio::test]
async fn a_node_that_now_shows_another_label_is_not_pressed() {
    let h = harness();
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    // the list re-rendered, and the node @e1 named holds another row's control
    h.world.lock().unwrap().elements[0] = button(1, "n10", "Edit row 2");
    assert!(matches!(
        h.browsers
            .act(":1", act("@e1", ActAction::Click, None))
            .await,
        Err(BrowserError::Stale(_))
    ));
    assert!(calls_of(&h, "browser_click").is_empty());
    // a fresh snap names it as it is
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    h.browsers
        .act(":1", act("@e1", ActAction::Click, None))
        .await
        .unwrap();
}

#[tokio::test]
async fn an_answer_past_its_cap_is_refused() {
    let h = harness();
    let label = "x".repeat(crate::screen::MAX_ANSWER_BYTES);
    h.world.lock().unwrap().elements = vec![button(1, "n10", &label)];
    let err = h
        .browsers
        .snap(":1", SnapRequest::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, BrowserError::Backend(m) if m.contains("answer to browser_get_state is more than")),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_page_off_the_list_shows_its_controls_and_none_of_what_they_hold() {
    let h = harness();
    h.world.lock().unwrap().elements.push(
        json!({"index": 6, "region": "n60", "role": "checkbox", "label": "Gift wrap",
               "input_type": "checkbox", "checked": true, "expanded": false,
               "selected_item": true, "aria_current": "page", "actions": ["toggle"]}),
    );
    let asked = |list: &str| SnapRequest {
        values: true,
        allow_domains: Some(vec![list.into()]),
        ..Default::default()
    };
    // the page is shop.example's: a list that holds it shows the email
    let on = h.browsers.snap(":1", asked("shop.example")).await.unwrap();
    assert_eq!(on.elements[1].value.as_deref(), Some("kid@example.com"));
    assert_eq!(on.elements[5].checked, Some(true));
    assert_eq!(on.elements[5].selected, Some(true));
    assert_eq!(on.elements[5].current.as_deref(), Some("page"));
    // another Bot's list doesn't hold it: the controls come, and nothing they hold
    let off = h.browsers.snap(":1", asked("bank.example")).await.unwrap();
    assert_eq!(off.elements.len(), on.elements.len());
    assert_eq!(off.elements[1].label, "Email");
    for e in &off.elements {
        assert!(
            e.value.is_none()
                && !e.value_truncated
                && e.checked.is_none()
                && e.expanded.is_none()
                && e.selected.is_none()
                && e.current.is_none(),
            "{e:?}"
        );
    }
    assert!(!off.text.contains("kid@example.com"), "{}", off.text);
    // values with no list, or an empty one, would read whatever page is current: refused
    for none in [None, Some(Vec::new())] {
        let bare = SnapRequest {
            values: true,
            allow_domains: none,
            ..Default::default()
        };
        let refused = h.browsers.snap(":1", bare).await;
        assert!(
            matches!(&refused, Err(BrowserError::Invalid(m)) if m.contains("values needs allow_domains")),
            "{refused:?}"
        );
    }
    // a list that isn't one is refused, as on act
    assert!(matches!(
        h.browsers.snap(":1", asked("not a host")).await,
        Err(BrowserError::Invalid(_))
    ));
}

/// A label with a character that renders as nothing is the same label to box-browser as to
/// ultra-instinct's guard: the press is asked about, and its token works, whichever look carried
/// the character.
#[tokio::test]
async fn a_zero_width_character_neither_slips_past_a_confirm_nor_spoils_its_token() {
    let h = harness();
    h.world.lock().unwrap().elements[3] = button(4, "n40", "Pa\u{200B}y $18");
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let Err(BrowserError::ConfirmRequired { token, .. }) = h
        .browsers
        .act(":1", act("@e4", ActAction::Click, None))
        .await
    else {
        panic!("a Pay click with a zero-width space went through");
    };
    assert!(calls_of(&h, "browser_click").is_empty());
    // the page drops the character before the press: the same label, so the ref and the token
    // both still hold
    h.world.lock().unwrap().elements[3] = button(4, "n40", "Pay $18");
    let mut approved = act("@e4", ActAction::Click, None);
    approved.confirm = Some(token);
    h.browsers.act(":1", approved).await.unwrap();
    assert_eq!(calls_of(&h, "browser_click").len(), 1);

    // snapped clean, pressed with a soft hyphen in it: asked about, not stale, not pressed
    h.browsers.snap(":1", SnapRequest::default()).await.unwrap();
    h.world.lock().unwrap().elements[3] = button(4, "n40", "P\u{AD}ay $18");
    let asked = h
        .browsers
        .act(":1", act("@e4", ActAction::Click, None))
        .await;
    assert!(
        matches!(asked, Err(BrowserError::ConfirmRequired { .. })),
        "{asked:?}"
    );
    assert_eq!(calls_of(&h, "browser_click").len(), 1);
}

/// ultra-instinct's real `Server`, as `seam::open` builds it, on a replayed page whose Pay button's
/// label carries a zero-width space: the label comes clean from the source, the click is asked
/// about, and the token lets that click through ultra-instinct's own guard as well.
#[tokio::test]
async fn ultra_instinct_s_labels_come_clean_and_a_zero_width_pay_is_confirmed() {
    use ultra_instinct_cdp::script::{Control, PageSpec, ScriptBuilder};
    use ultra_instinct_cdp::{CdpTransport, ReplayTransport};

    struct Replay;
    impl Opener for Replay {
        fn open(&self, _display: &str) -> Result<Box<dyn Tools>, BrowserError> {
            let page = PageSpec::of(
                &[
                    Control::text_field(20, 200, "Email", (400.0, 200.0, 200.0, 32.0)),
                    Control::button(10, 100, "Pa\u{200B}y $18", (400.0, 300.0, 80.0, 32.0)),
                ],
                "https://shop.example/cart",
                "Cart",
            );
            // the snap; the first act's look, refused by box-browser before any press; the second
            // act's look, then the press's two looks (its gate and its ticket) and the press
            let script = ScriptBuilder::new()
                .observe_compact(&page)
                .observe_compact(&page)
                .observe_compact(&page)
                .observe_compact(&page)
                .observe_compact(&page)
                .pointer_click()
                .to_json();
            let server = ultra_instinct_mcp::Server::with_connector(move |_url| {
                ReplayTransport::parse(&script).map(|t| Box::new(t) as Box<dyn CdpTransport>)
            })
            .require_snapshot(true)
            .with_tool_allowlist(seam::ALLOWED);
            Ok(Box::new(seam::ServerTools::new(server)))
        }
    }
    let browsers = Browsers::new(
        true,
        Rules::default(),
        Arc::new(Replay),
        std::env::temp_dir(),
    );
    let snap = browsers.snap(":1", SnapRequest::default()).await.unwrap();
    let pay = snap
        .elements
        .iter()
        .find(|e| e.role == "button")
        .unwrap_or_else(|| panic!("no button: {snap:?}"));
    assert_eq!(pay.label, "Pay $18", "{snap:?}");
    let pay = pay.reference.clone();
    let Err(BrowserError::ConfirmRequired { label, token }) =
        browsers.act(":1", act(&pay, ActAction::Click, None)).await
    else {
        panic!("the Pay click went through unasked");
    };
    assert_eq!(label, "Pay $18");
    let mut approved = act(&pay, ActAction::Click, None);
    approved.confirm = Some(token);
    let done = browsers.act(":1", approved).await;
    assert!(matches!(&done, Ok(d) if d.ok), "{done:?}");
}

#[tokio::test]
async fn with_box_chrome_off_the_main_screen_is_closed_and_a_bots_own_is_served() {
    let h = harness();
    let main_closed = h.browsers.clone().with_main(MainScreen::Closed(Some(1)));
    assert!(matches!(
        main_closed.snap(":1", SnapRequest::default()).await,
        Err(BrowserError::Disabled)
    ));
    assert!(matches!(
        main_closed.snap(":1.0", SnapRequest::default()).await,
        Err(BrowserError::Disabled)
    ));
    assert_eq!(
        h.opener.1.load(Ordering::SeqCst),
        0,
        "nothing opened the main screen's browser"
    );
    assert!(main_closed.snap(":3", SnapRequest::default()).await.is_ok());
    // a main screen that can't be named closes every screen
    let unknown = h.browsers.clone().with_main(MainScreen::Closed(None));
    assert!(matches!(
        unknown.snap(":3", SnapRequest::default()).await,
        Err(BrowserError::Disabled)
    ));
    // with BOX_CHROME on, the main screen is served as before
    assert!(h.browsers.snap(":1", SnapRequest::default()).await.is_ok());
}
