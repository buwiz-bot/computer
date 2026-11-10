//! A screen's AT-SPI bus, and the [`Source`] that reads it.
//!
//! Each screen has its own accessibility bus, run by its desktop user (`docker/box-a11y-bus`), at
//! `/run/box/x/<N>/run/a11y-bus`. That folder is the desktop user's, open to `boxsvc` (box-exec)
//! and no one else, so a command's user can't reach the socket, and the bus's policy lets in only
//! the desktop user and `boxsvc` besides. Anyone could have bound the name of a screen that's
//! down, though not in that folder, so as with X (`box_desktop::screen_stream`) the listener is
//! checked to be the screen's own desktop user before anything is said to it. A desktop's apps
//! are not trusted: every answer is bounded by the caller's limits and time.

use std::path::PathBuf;
use std::time::Duration;

use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::proxy::editable_text::EditableTextProxy;
use atspi::proxy::text::TextProxy;
use atspi::{CoordType, Interface, InterfaceSet, ObjectRefOwned, Role, State, StateSet};
use tokio::net::UnixStream;
use zbus::proxy::CacheProperties;

use crate::tree::{Bounds, Handle, Info, Source, MAX_TEXT_CHARS};
use crate::A11yError;

const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
/// How long one call to an application may take. A hung application costs this, not the walk.
const CALL_PATIENCE: Duration = Duration::from_millis(1000);
const CONNECT_PATIENCE: Duration = Duration::from_millis(2000);

/// Where screen `slot`'s bus listens.
pub fn bus_path(slot: u16) -> PathBuf {
    PathBuf::from(format!("/run/box/x/{slot}/run/a11y-bus"))
}

/// A connection to one screen's accessibility bus.
pub struct Screen {
    conn: zbus::Connection,
}

impl Screen {
    pub async fn connect(display: &str) -> Result<Self, A11yError> {
        let slot = box_desktop::display_number(display)
            .ok_or_else(|| A11yError::Invalid(format!("display must be :N, not {display:?}")))?;
        let path = bus_path(slot);
        let unavailable = |why: String| {
            A11yError::Unavailable(format!("{display} has no accessibility bus: {why}"))
        };
        let connect = async {
            let stream = UnixStream::connect(&path)
                .await
                .map_err(|err| unavailable(format!("{}: {err}", path.display())))?;
            check_listener(&stream, slot).map_err(unavailable)?;
            zbus::connection::Builder::unix_stream(stream)
                .method_timeout(CALL_PATIENCE)
                .build()
                .await
                .map_err(|err| unavailable(err.to_string()))
        };
        let conn = tokio::time::timeout(CONNECT_PATIENCE, connect)
            .await
            .map_err(|_| unavailable("it did not answer in time".into()))??;
        Ok(Self { conn })
    }

    async fn accessible(&self, node: &Handle) -> Result<AccessibleProxy<'static>, A11yError> {
        AccessibleProxy::builder(&self.conn)
            .destination(node.bus.clone())
            .and_then(|b| b.path(node.path.clone()))
            .map_err(gone)?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(gone)
    }

    /// Runs `action` on `node`. `text` is for [`ElementAction::SetText`] alone. For
    /// [`ElementAction::Click`] nothing is run here: the answer is where to click, the middle of
    /// the element as it is now, and the caller clicks it.
    pub async fn act(
        &self,
        node: &Handle,
        action: crate::ElementAction,
        text: Option<&str>,
    ) -> Result<crate::Acted, A11yError> {
        use crate::{Acted, ElementAction};

        let accessible = self.accessible(node).await?;
        let interfaces = accessible.get_interfaces().await.map_err(gone)?;
        let states = accessible.get_state().await.map_err(gone)?;
        if states.contains(State::Defunct) {
            return Err(A11yError::Stale(
                "the element is gone: read the tree again".into(),
            ));
        }
        let unsupported = |what: &str| {
            A11yError::Unsupported(format!(
                "this element can't be {what} through its accessibility interface; try action \"click\""
            ))
        };
        let mut acted = Acted::default();
        let done = match action {
            ElementAction::Press => {
                if !interfaces.contains(Interface::Action) {
                    return Err(unsupported("pressed"));
                }
                let proxy = proxy::<ActionProxy>(&self.conn, node).await?;
                let names: Vec<String> = proxy
                    .get_actions()
                    .await
                    .map_err(gone)?
                    .into_iter()
                    .map(|a| a.name)
                    .collect();
                let index = crate::press_action(&names).ok_or_else(|| {
                    A11yError::Unsupported(format!(
                        "this element has no press-like action (it has {names:?}); try action \"click\""
                    ))
                })?;
                acted.performed = Some(crate::tree::action_name(index, &names[index]));
                proxy.do_action(index as i32).await.map_err(gone)?
            }
            ElementAction::Focus => {
                if !interfaces.contains(Interface::Component) {
                    return Err(unsupported("focused"));
                }
                let proxy = proxy::<ComponentProxy>(&self.conn, node).await?;
                proxy.grab_focus().await.map_err(gone)?
            }
            ElementAction::SetText => {
                if !interfaces.contains(Interface::EditableText) {
                    return Err(unsupported("given text"));
                }
                let proxy = proxy::<EditableTextProxy>(&self.conn, node).await?;
                let text = text.unwrap_or_default();
                proxy.set_text_contents(text).await.map_err(gone)?
            }
            ElementAction::Click => {
                if !interfaces.contains(Interface::Component) {
                    return Err(A11yError::Unsupported(
                        "this element has no bounds to click".into(),
                    ));
                }
                let proxy = proxy::<ComponentProxy>(&self.conn, node).await?;
                let (x, y, width, height) =
                    proxy.get_extents(CoordType::Screen).await.map_err(gone)?;
                if width <= 0 || height <= 0 {
                    return Err(A11yError::Unsupported(
                        "this element takes up no space on the screen now".into(),
                    ));
                }
                acted.at = Some(crate::Point {
                    x: x.saturating_add(width / 2),
                    y: y.saturating_add(height / 2),
                });
                true
            }
        };
        if !done {
            return Err(A11yError::Refused(format!(
                "the application refused to {}",
                match action {
                    ElementAction::Press => "press it",
                    ElementAction::Focus => "focus it",
                    ElementAction::SetText => "set its text",
                    ElementAction::Click => "say where it is",
                }
            )));
        }
        Ok(acted)
    }
}

/// Whether what listens at the socket is screen `slot`'s own desktop user. Outside the box (no
/// `BOX_RUN_AS=agents`) there are no users to tell apart, as for X. In the box, a
/// `BOX_SHARED_DISPLAY` that isn't a display name refuses, as for X.
fn check_listener(stream: &UnixStream, slot: u16) -> Result<(), String> {
    if std::env::var("BOX_RUN_AS").as_deref() != Ok("agents") {
        return Ok(());
    }
    let expected = box_desktop::screen_owner(slot)?;
    let peer = stream.peer_cred().map_err(|err| err.to_string())?;
    if peer.uid() != expected {
        return Err("what listens there isn't this screen's desktop".into());
    }
    Ok(())
}

async fn proxy<'a, P>(conn: &zbus::Connection, node: &Handle) -> Result<P, A11yError>
where
    P: From<zbus::Proxy<'a>> + zbus::proxy::Defaults,
{
    zbus::proxy::Builder::<P>::new(conn)
        .destination(node.bus.clone())
        .and_then(|b| b.path(node.path.clone()))
        .map_err(gone)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(gone)
}

/// An error from an application. A node or an application that is no longer there is stale; a
/// method the element doesn't have is unsupported, since the element is still there and a fresh
/// tree would give the same ref; the rest is the backend's.
fn gone(err: zbus::Error) -> A11yError {
    let name = match &err {
        zbus::Error::MethodError(name, _, _) => Some(name.as_str().to_string()),
        zbus::Error::FDO(fdo) => match **fdo {
            zbus::fdo::Error::ServiceUnknown(_) => Some("ServiceUnknown".into()),
            zbus::fdo::Error::NameHasNoOwner(_) => Some("NameHasNoOwner".into()),
            zbus::fdo::Error::UnknownObject(_) => Some("UnknownObject".into()),
            zbus::fdo::Error::UnknownMethod(_) => Some("UnknownMethod".into()),
            _ => None,
        },
        _ => None,
    };
    let is = |what: &[&str]| {
        name.as_deref()
            .is_some_and(|name| what.iter().any(|w| name.ends_with(w)))
    };
    if is(&["ServiceUnknown", "NameHasNoOwner", "UnknownObject"]) {
        A11yError::Stale("the element is gone: read the tree again".into())
    } else if is(&["UnknownMethod"]) {
        A11yError::Unsupported(format!(
            "the element's application doesn't do this: {err}; try action \"click\""
        ))
    } else {
        A11yError::Backend(format!("the application did not answer: {err}"))
    }
}

fn handle(object: ObjectRefOwned) -> Option<Handle> {
    if object.is_null() {
        return None;
    }
    Some(Handle {
        bus: object.name_as_str()?.to_string(),
        path: object.path_as_str().to_string(),
    })
}

/// What a node must answer to be kept. A node that fails to say its states or its child count is
/// an error, which the walk counts as skipped: not a hidden node, or a leaf, it may not be.
fn essentials(
    name: zbus::Result<String>,
    states: zbus::Result<StateSet>,
    interfaces: zbus::Result<InterfaceSet>,
    child_count: zbus::Result<i32>,
) -> Result<(String, StateSet, InterfaceSet, i32), A11yError> {
    Ok((
        name.map_err(gone)?,
        states.map_err(gone)?,
        interfaces.map_err(gone)?,
        child_count.map_err(gone)?,
    ))
}

/// Whether a node's text may be read: only when it has the Text interface and a role that is known
/// and isn't a password field's. A role that can't be read, or reads as unknown, could be one.
fn may_read_text(interfaces: InterfaceSet, role: Option<Role>) -> bool {
    interfaces.contains(Interface::Text)
        && role
            .is_some_and(|role| !matches!(role, Role::PasswordText | Role::Unknown | Role::Invalid))
}

impl Source for Screen {
    async fn applications(&self) -> Result<Vec<Handle>, A11yError> {
        let root = Handle {
            bus: REGISTRY.into(),
            path: ROOT.into(),
        };
        let children = self
            .accessible(&root)
            .await?
            .get_children()
            .await
            .map_err(|err| {
                A11yError::Backend(format!("the accessibility registry did not answer: {err}"))
            })?;
        Ok(children.into_iter().filter_map(handle).collect())
    }

    async fn info(&self, node: &Handle) -> Result<Info, A11yError> {
        let accessible = self.accessible(node).await?;
        let (name, description, states, interfaces, child_count, role) = futures_util::join!(
            accessible.name(),
            accessible.description(),
            accessible.get_state(),
            accessible.get_interfaces(),
            accessible.child_count(),
            accessible.get_role(),
        );
        let (name, states, interfaces, child_count) =
            essentials(name, states, interfaces, child_count)?;
        // A role this crate doesn't know yet is no reason to lose the node, but it is a reason not
        // to read its text: it could be a password field.
        let role = role.ok();
        let states = states.iter().map(|s| s.to_static_str()).collect();

        let bounds = async {
            if !interfaces.contains(Interface::Component) {
                return None;
            }
            let component = proxy::<ComponentProxy>(&self.conn, node).await.ok()?;
            let (x, y, width, height) = component.get_extents(CoordType::Screen).await.ok()?;
            Some(Bounds {
                x,
                y,
                width,
                height,
            })
        };
        let actions = async {
            if !interfaces.contains(Interface::Action) {
                return Vec::new();
            }
            let Ok(action) = proxy::<ActionProxy>(&self.conn, node).await else {
                return Vec::new();
            };
            action
                .get_actions()
                .await
                .map(|actions| actions.into_iter().map(|a| a.name).collect())
                .unwrap_or_default()
        };
        let text = async {
            if !may_read_text(interfaces, role) {
                return None;
            }
            let text = proxy::<TextProxy>(&self.conn, node).await.ok()?;
            let count = text.character_count().await.ok()?;
            let end = count.clamp(0, MAX_TEXT_CHARS as i32 + 1);
            text.get_text(0, end).await.ok()
        };
        let (bounds, actions, text) = futures_util::join!(bounds, actions, text);
        Ok(Info {
            role: role.map_or("unknown", |role| role.name()).to_string(),
            name,
            description: description.unwrap_or_default(),
            states,
            bounds,
            actions,
            text,
            child_count: usize::try_from(child_count).unwrap_or(0),
        })
    }

    async fn children(&self, node: &Handle) -> Result<Vec<Handle>, A11yError> {
        let children = self
            .accessible(node)
            .await?
            .get_children()
            .await
            .map_err(gone)?;
        Ok(children.into_iter().filter_map(handle).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_read_only_for_a_known_role_that_is_not_a_password() {
        let text = InterfaceSet::new(Interface::Text);
        assert!(may_read_text(text, Some(Role::Entry)));
        // A role that couldn't be read.
        assert!(!may_read_text(text, None));
        assert!(!may_read_text(text, Some(Role::PasswordText)));
        assert!(!may_read_text(text, Some(Role::Unknown)));
        assert!(!may_read_text(text, Some(Role::Invalid)));
        assert!(!may_read_text(InterfaceSet::empty(), Some(Role::Entry)));
    }

    #[test]
    fn a_node_that_cannot_say_its_states_or_children_is_an_error() {
        fn failed<T>() -> zbus::Result<T> {
            Err(zbus::Error::Failure("no".into()))
        }
        let name = || Ok("OK".to_string());
        let states = || Ok(StateSet::new(State::Showing));
        let interfaces = || Ok(InterfaceSet::empty());
        assert!(essentials(name(), states(), interfaces(), Ok(2)).is_ok());
        assert!(essentials(name(), failed(), interfaces(), Ok(2)).is_err());
        assert!(essentials(name(), states(), interfaces(), failed()).is_err());
    }

    #[test]
    fn only_a_missing_node_or_application_is_stale() {
        let fdo = |err: zbus::fdo::Error| gone(zbus::Error::FDO(Box::new(err)));
        let named = |name: &str| {
            let name = zbus::names::OwnedErrorName::try_from(name.to_string()).unwrap();
            let reply = zbus::Message::method_call("/", "Get")
                .unwrap()
                .build(&())
                .unwrap();
            gone(zbus::Error::MethodError(name, None, reply))
        };
        assert!(matches!(
            fdo(zbus::fdo::Error::UnknownObject("x".into())),
            A11yError::Stale(_)
        ));
        assert!(matches!(
            fdo(zbus::fdo::Error::ServiceUnknown("x".into())),
            A11yError::Stale(_)
        ));
        assert!(matches!(
            fdo(zbus::fdo::Error::NameHasNoOwner("x".into())),
            A11yError::Stale(_)
        ));
        assert!(matches!(
            fdo(zbus::fdo::Error::UnknownMethod("x".into())),
            A11yError::Unsupported(_)
        ));
        assert!(matches!(
            named("org.freedesktop.DBus.Error.UnknownMethod"),
            A11yError::Unsupported(_)
        ));
        assert!(matches!(
            named("org.freedesktop.DBus.Error.UnknownObject"),
            A11yError::Stale(_)
        ));
        assert!(matches!(
            fdo(zbus::fdo::Error::Failed("x".into())),
            A11yError::Backend(_)
        ));
    }
}
