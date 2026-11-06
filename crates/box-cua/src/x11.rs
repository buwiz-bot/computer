//! Persistent X11 connections: XTEST input and GetImage screenshots.
//!
//! Input and capture use **separate** connections so a PNG encode cannot
//! stall clicks. The async `ACTUATOR` mutex in `lib.rs` still serializes
//! pointer gestures (click gap, drag grab); it is never held across
//! screenshot work.
//!
//! Both paths are blocking X11 round-trips, so both run on `spawn_blocking`
//! rather than on a tokio worker. Input additionally carries a deadline:
//! x11rb has no reply deadline of its own, and input runs while `lib.rs`
//! holds `ACTUATOR`, so a wedged or paused X server would otherwise park a
//! worker for as long as the server stays quiet and leave every later CUA
//! request queued behind a lock that is never released.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use arrayvec::ArrayVec;
use smallvec::SmallVec;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::ReplyError;
use x11rb::image::{BitsPerPixel, Image, ImageOrder};
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ConnectionExt as _, KeyButMask, Keycode, Keysym, Window,
};
use x11rb::protocol::xtest::{self, ConnectionExt as _};
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::NONE;

use crate::encode;
use crate::keys::{
    char_to_keysym, parse_key_sequence, XK_ALT_L, XK_CONTROL_L, XK_SHIFT_L, XK_SUPER_L,
};
use crate::observe::{Focus, FocusState, WindowRef};
use crate::{CuaConfig, CuaError, CuaPoint, KeyAction, MAX_COLLECTED_POINTS};

/// The XTEST connection and its cached keymap. A `tokio::sync::Mutex`, not
/// a `std::sync::Mutex`, so that *acquiring* it can carry a deadline: a
/// worker parked inside a wedged X server keeps this guard until the server
/// answers, and every other caller has to learn that in bounded time rather
/// than queue behind it.
type XInput = tokio::sync::Mutex<Option<InputState>>;

static INPUT: XInput = XInput::const_new(None);
static SHOT: Mutex<Option<ShotConn>> = Mutex::new(None);

/// Observation gets its own connection, for the same reason capture has one:
/// a read the X server is slow to answer must not park the socket the next
/// keystroke needs.
///
/// It is an `InputConn` even though it sends no XTEST, which costs one
/// version check at connect time and buys the whole deadline, reconnect and
/// lock-with-a-bound apparatus that `with_native_on` already implements. A
/// display without XTEST has no working CUA input either, so declining to
/// observe there loses nothing a caller could have used.
static OBSERVE: XInput = XInput::const_new(None);

/// How long one XTEST call may wait on the X server before the request is
/// failed. A healthy round-trip over the guest's unix socket is measured in
/// microseconds (`benches/hotpath.rs`: 1.6 µs for motion+flush, 4 ms for a
/// whole 1280×800 screenshot), so two seconds is three orders of magnitude
/// of headroom for a merely busy server while still bounding a wedged one
/// to something a caller can retry. It also sits under the 2.5 s
/// `XDOTOOL_TIMEOUT`, so a native attempt plus its fork/exec fallback still
/// fit inside one HTTP request.
const XTEST_DEADLINE: Duration = Duration::from_millis(2_000);

/// Added to `XTEST_DEADLINE` per character of `type`, because one call types
/// the whole string and a keysym that is missing from the keymap costs a
/// GetKeyboardMapping round-trip and a MappingNotify to every X client for
/// that one character. 5 ms each is ~1000× the mapped path and ~50× a
/// scratch remap, so a long non-Latin paste keeps working and a wedged
/// server still fails.
const TYPE_CHAR_BUDGET: Duration = Duration::from_millis(5);

/// How long a caller waits for the XTEST connection itself. `lib.rs` already
/// serializes every CUA verb behind `ACTUATOR`, so finding this lock held
/// means an earlier call is still parked inside the X server; the wait is
/// only long enough for one that is finishing right now.
const INPUT_LOCK_WAIT: Duration = Duration::from_millis(250);

/// How long one observation may wait on the X server.
///
/// Far shorter than `XTEST_DEADLINE`, because the two are not worth the same:
/// a receipt that says "not observed" is still a correct receipt, whereas a
/// recipe that runs slower because it was watching itself is not. Observation
/// holds its own connection, so giving up here cannot strand the input path.
const OBSERVE_DEADLINE: Duration = Duration::from_millis(400);

/// How far the box will walk an X window tree looking for a client window.
///
/// The chain under a pointer is root → frame → client → widget, four deep on
/// this desktop even for Chromium. Sixteen is room for a stranger toolkit and
/// still a hard bound on the round-trips one observation can cost.
const MAX_WINDOW_DEPTH: usize = 16;

/// Longest property the box will read, in 4-byte units — 1 KiB, which holds
/// any `WM_CLASS` and any page title worth putting on a receipt. The bound is
/// on the X reply, so a window with a pathological title cannot make an
/// observation expensive.
const MAX_PROP_WORDS: u32 = 256;

/// `GetInputFocus` answers with a window id, except for two reserved values
/// the protocol gives special meaning: 0 is *None* (the server discards
/// keyboard events) and 1 is *PointerRoot* (they follow the pointer).
const FOCUS_NONE: Window = 0;
const FOCUS_POINTER_ROOT: Window = 1;

/// What one XTEST attempt did.
pub(crate) enum Native<T> {
    /// The server answered. `Err` is a protocol or socket failure, which is
    /// worth retrying through xdotool.
    Ran(Result<T, CuaError>),
    /// XTEST is not usable on this display; the caller must use xdotool.
    Unavailable,
    /// The server did not answer inside the deadline, or an earlier call is
    /// still parked inside it. xdotool drives the same server and would only
    /// spend its own timeout learning that, so callers report this instead
    /// of falling back.
    Wedged(CuaError),
}

struct InputState {
    display: String,
    backend: Backend,
}

#[allow(clippy::large_enum_variant)]
enum Backend {
    Native(InputConn),
    /// XTEST unavailable; callers use xdotool.
    Cli,
}

/// Persistent XTEST connection. Vecs first, then packed keycodes — no
/// `repr(C)` / `packed` (this is not FFI).
pub(crate) struct InputConn {
    conn: RustConnection,
    keysyms: Vec<Keysym>,
    mod_keycodes: Vec<Keycode>,
    root: Window,
    /// EWMH title atoms, interned once per connection so that reading a
    /// window's name costs one round-trip rather than three. `NONE` when the
    /// server would not intern them, which drops the title back to `WM_NAME`.
    net_wm_name: Atom,
    utf8_string: Atom,
    min_keycode: Keycode,
    keysyms_per: u8,
    shift_l: Keycode,
    control_l: Keycode,
    alt_l: Keycode,
    super_l: Keycode,
    keycodes_per_mod: u8,
    scratch: Keycode,
}

struct ShotConn {
    conn: RustConnection,
    display: String,
    rgb: Vec<u8>,
    root: Window,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
}

struct MappingGuard<'a> {
    conn: &'a RustConnection,
    keycode: Keycode,
    per: u8,
    old: Vec<Keysym>,
}

impl Drop for MappingGuard<'_> {
    fn drop(&mut self) {
        let _ = self
            .conn
            .change_keyboard_mapping(1, self.keycode, self.per, &self.old);
        let _ = self.conn.flush();
    }
}

/// `DisplayDown`, not `Tool`: `box-exec` maps it to 503 `display_unavailable`,
/// which is what "the X server stopped answering" means. `Tool` is a 502 and
/// reads like the input landed and did nothing.
fn wedged(dpy: &str, detail: &str) -> CuaError {
    CuaError::DisplayDown(format!("{dpy} ({detail})"))
}

fn lock_shot() -> std::sync::MutexGuard<'static, Option<ShotConn>> {
    SHOT.lock().unwrap_or_else(|e| e.into_inner())
}

fn x_err(err: impl std::fmt::Display) -> CuaError {
    CuaError::Tool(format!("x11: {err}"))
}

/// Intern an atom, or `NONE` if the server would not. A missing atom is not
/// worth failing a connection over: it only costs the caller a fallback.
fn intern(conn: &RustConnection, name: &[u8]) -> Atom {
    conn.intern_atom(false, name)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.atom)
        .unwrap_or(NONE)
}

/// The `0x…` form `wmctrl -lx` prints and `GET /v1/desktop/windows` echoes,
/// so an id on a receipt can be matched against one from either.
fn window_id(window: Window) -> String {
    format!("0x{window:08x}")
}

/// `WM_CLASS` is two NUL-terminated strings, instance then class. `wmctrl`
/// joins them with a dot and the rest of the box has followed it, so a
/// receipt reports the same spelling rather than a second one.
fn format_wm_class(bytes: &[u8]) -> Option<String> {
    let mut parts = bytes
        .split(|b| *b == 0)
        .map(|part| String::from_utf8_lossy(part))
        .filter(|part| !part.is_empty());
    let instance = parts.next()?;
    match parts.next() {
        Some(class) => Some(format!("{instance}.{class}")),
        None => Some(instance.into_owned()),
    }
}

/// X text properties are NUL-terminated and `WM_NAME` is latin-1, so this is
/// lossy on purpose. An empty result is reported as absent, because "the
/// window has no title" and "the title did not come back" must not read the
/// same way.
fn prop_text(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn connect_state(dpy: &str) -> InputState {
    match InputConn::connect(dpy) {
        Ok(conn) => {
            tracing::info!(display = dpy, "cua xtest connected");
            InputState {
                display: dpy.to_string(),
                backend: Backend::Native(conn),
            }
        }
        Err(err) => {
            tracing::warn!(display = dpy, error = %err, "cua xtest unavailable; xdotool fallback");
            InputState {
                display: dpy.to_string(),
                backend: Backend::Cli,
            }
        }
    }
}

/// Run `op` on a live XTEST connection, off the tokio workers the way the
/// screenshot path already is, and inside `budget`.
pub(crate) async fn with_native<T: Send + 'static>(
    dpy: &str,
    budget: Duration,
    op: impl FnOnce(&mut InputConn) -> Result<T, CuaError> + Send + 'static,
) -> Native<T> {
    with_native_on(&INPUT, dpy, budget, op).await
}

/// The connection is a parameter so a test can wedge one of its own instead
/// of parking the process-wide connection for every later call.
async fn with_native_on<T: Send + 'static>(
    input: &'static XInput,
    dpy: &str,
    budget: Duration,
    op: impl FnOnce(&mut InputConn) -> Result<T, CuaError> + Send + 'static,
) -> Native<T> {
    let Ok(mut guard) = tokio::time::timeout(INPUT_LOCK_WAIT, input.lock()).await else {
        tracing::error!(
            display = dpy,
            wait_ms = INPUT_LOCK_WAIT.as_millis() as u64,
            "cua xtest busy; an earlier call is still inside the X server"
        );
        return Native::Wedged(wedged(
            dpy,
            "XTEST busy; an earlier call is still inside the X server",
        ));
    };
    let display = dpy.to_string();
    // Connecting runs here too, not just `op`: `InputConn::connect` is three
    // round-trips and a paused server blocks the first one. `guard` moves
    // into the task so the lock is released by whichever thread finishes the
    // work, even when the caller below has already given up on it.
    let job = tokio::task::spawn_blocking(move || {
        let state = guard.get_or_insert_with(|| connect_state(&display));
        if state.display != display {
            *state = connect_state(&display);
        }
        match &mut state.backend {
            Backend::Cli => None,
            Backend::Native(conn) => {
                let result = op(conn);
                if result.is_err() {
                    // Drop a broken socket so the next call reconnects (or falls back).
                    *guard = None;
                }
                Some(result)
            }
        }
    });
    match tokio::time::timeout(budget, job).await {
        Ok(Ok(Some(result))) => Native::Ran(result),
        Ok(Ok(None)) => Native::Unavailable,
        Ok(Err(err)) => Native::Ran(Err(CuaError::Tool(format!("xtest worker: {err}")))),
        Err(_) => {
            // A blocking task cannot be cancelled: it stays parked in the X
            // server and keeps the guard, which is exactly how the next
            // caller finds out fast. Say so loudly — a keystroke that never
            // reached the server must not look like one that landed.
            tracing::error!(
                display = dpy,
                ms = budget.as_millis() as u64,
                "cua xtest timed out; the X server is not answering"
            );
            Native::Wedged(wedged(
                dpy,
                &format!("XTEST timed out after {}ms", budget.as_millis()),
            ))
        }
    }
}

pub(crate) fn capture_png(display: &str, width: u32, height: u32) -> Result<Vec<u8>, CuaError> {
    let mut guard = lock_shot();
    let need_new = match guard.as_ref() {
        Some(c) => c.display != display,
        None => true,
    };
    if need_new {
        *guard = Some(ShotConn::connect(display)?);
    }
    // SAFETY: `need_new` inserted `Some`, otherwise a previous `Some` was kept.
    let shot = unsafe { guard.as_mut().unwrap_unchecked() };
    match shot.capture(width, height) {
        Ok(png) => Ok(png),
        Err(err) => {
            *guard = None;
            Err(err)
        }
    }
}

/// A connection to `display`. In the box the handshake, and so the cookie, goes over the very stream
/// the kernel vouched for, and to nothing else: `box_desktop::screen_stream`.
fn x_connect(display: &str) -> Result<(RustConnection, usize), CuaError> {
    if !crate::verify_servers() {
        return x11rb::connect(Some(display)).map_err(x_err);
    }
    let down = || CuaError::DisplayDown(display.to_string());
    let stream = box_desktop::screen_stream(display).ok_or_else(down)?;
    let slot = box_desktop::display_number(display).ok_or_else(down)?;
    let (stream, (family, address)) = DefaultStream::from_unix_stream(stream).map_err(x_err)?;
    let (name, data) = x11rb::reexports::x11rb_protocol::xauth::get_auth(family, &address, slot)
        .ok()
        .flatten()
        .ok_or_else(down)?;
    let conn =
        RustConnection::connect_to_stream_with_auth_info(stream, 0, name, data).map_err(x_err)?;
    Ok((conn, 0))
}

impl ShotConn {
    fn connect(display: &str) -> Result<Self, CuaError> {
        let (conn, screen) = x_connect(display)?;
        let setup = conn.setup();
        let screen = setup.roots.get(screen).ok_or_else(|| x_err("no screen"))?;
        let root = screen.root;
        let visual_id = screen.root_visual;
        let mut red_mask = 0x00ff_0000;
        let mut green_mask = 0x0000_ff00;
        let mut blue_mask = 0x0000_00ff;
        for depth in &screen.allowed_depths {
            for vis in &depth.visuals {
                if vis.visual_id == visual_id {
                    red_mask = vis.red_mask;
                    green_mask = vis.green_mask;
                    blue_mask = vis.blue_mask;
                }
            }
        }
        Ok(Self {
            conn,
            display: display.to_string(),
            rgb: Vec::new(),
            root,
            red_mask,
            green_mask,
            blue_mask,
        })
    }

    fn capture(&mut self, width: u32, height: u32) -> Result<Vec<u8>, CuaError> {
        let w = u16::try_from(width).map_err(|_| x_err("width"))?;
        let h = u16::try_from(height).map_err(|_| x_err("height"))?;
        let started = Instant::now();
        let (image, _) = Image::get(&self.conn, self.root, 0, 0, w, h).map_err(x_err)?;
        let get_ms = started.elapsed().as_millis() as u64;
        let needed = width as usize * height as usize * 3;
        if self.rgb.len() != needed {
            self.rgb.clear();
            self.rgb.resize(needed, 0);
        }
        let bpp = match image.bits_per_pixel() {
            BitsPerPixel::B32 => 32,
            BitsPerPixel::B24 => 24,
            other => {
                return Err(CuaError::Tool(format!(
                    "unsupported screenshot bits_per_pixel {other:?}"
                )))
            }
        };
        let stride = if h == 0 {
            0
        } else {
            image.data().len() / h as usize
        };
        let lsb = image.byte_order() == ImageOrder::LsbFirst;
        encode::zpixmap_to_rgb(
            image.data(),
            image.width() as u32,
            image.height() as u32,
            stride,
            lsb,
            bpp,
            self.red_mask,
            self.green_mask,
            self.blue_mask,
            &mut self.rgb,
        )?;
        let png = encode::encode_png_rgb(width, height, &self.rgb)?;
        let ms = started.elapsed().as_millis() as u64;
        if ms >= 50 {
            tracing::warn!(ms, get_ms, bytes = png.len(), "screenshot slow");
        } else {
            tracing::debug!(ms, get_ms, bytes = png.len(), "screenshot");
        }
        Ok(png)
    }
}

impl InputConn {
    fn connect(display: &str) -> Result<Self, CuaError> {
        let (conn, screen) = x_connect(display)?;
        conn.extension_information(xtest::X11_EXTENSION_NAME)
            .map_err(x_err)?
            .ok_or_else(|| CuaError::Tool("XTEST extension missing on display".into()))?;
        conn.xtest_get_version(2, 1)
            .map_err(x_err)?
            .reply()
            .map_err(x_err)?;
        let setup = conn.setup();
        let root = setup
            .roots
            .get(screen)
            .ok_or_else(|| x_err("no screen"))?
            .root;
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let count = max_keycode.saturating_sub(min_keycode).saturating_add(1);
        let map = conn
            .get_keyboard_mapping(min_keycode, count)
            .map_err(x_err)?
            .reply()
            .map_err(x_err)?;
        let mods = conn
            .get_modifier_mapping()
            .map_err(x_err)?
            .reply()
            .map_err(x_err)?;
        let keycodes_per_mod = mods.keycodes_per_modifier();
        // Interned rather than looked up, because a fresh X server has not
        // seen `_NET_WM_NAME` until an EWMH client asks for it, and the box
        // may connect before Chromium starts. Creating an atom nobody else
        // has asked for yet is harmless.
        let net_wm_name = intern(&conn, b"_NET_WM_NAME");
        let utf8_string = intern(&conn, b"UTF8_STRING");
        let mut this = Self {
            conn,
            keysyms: map.keysyms,
            mod_keycodes: mods.keycodes,
            root,
            net_wm_name,
            utf8_string,
            min_keycode,
            keysyms_per: map.keysyms_per_keycode,
            shift_l: 0,
            control_l: 0,
            alt_l: 0,
            super_l: 0,
            keycodes_per_mod,
            scratch: max_keycode,
        };
        this.shift_l = this.lookup_keycode(XK_SHIFT_L).unwrap_or(0);
        this.control_l = this.lookup_keycode(XK_CONTROL_L).unwrap_or(0);
        this.alt_l = this.lookup_keycode(XK_ALT_L).unwrap_or(0);
        this.super_l = this.lookup_keycode(XK_SUPER_L).unwrap_or(0);
        Ok(this)
    }

    fn lookup_keycode(&self, keysym: Keysym) -> Option<Keycode> {
        self.lookup(keysym).map(|(kc, _)| kc)
    }

    /// One property's bytes, or `None` when the window does not carry it.
    ///
    /// A window can be destroyed between being found and being asked about,
    /// and that race has to read as "not observed" rather than as a failure:
    /// `with_native_on` drops the shared connection whenever an op returns
    /// `Err`, and a window that closed while the box was looking at it is no
    /// reason to reconnect. A connection-level error still is.
    fn prop(&self, window: Window, property: Atom, ty: Atom) -> Result<Option<Vec<u8>>, CuaError> {
        let cookie = self
            .conn
            .get_property(false, window, property, ty, 0, MAX_PROP_WORDS)
            .map_err(x_err)?;
        match cookie.reply() {
            Ok(reply) if !reply.value.is_empty() => Ok(Some(reply.value)),
            Ok(_) => Ok(None),
            Err(ReplyError::X11Error(_)) => Ok(None),
            Err(err) => Err(x_err(err)),
        }
    }

    fn wm_class(&self, window: Window) -> Result<Option<String>, CuaError> {
        match self.prop(window, AtomEnum::WM_CLASS.into(), AtomEnum::STRING.into())? {
            Some(bytes) => Ok(format_wm_class(&bytes)),
            None => Ok(None),
        }
    }

    /// `_NET_WM_NAME` first because it is UTF-8 and is what a window manager
    /// and Chromium both keep current; `WM_NAME` is the latin-1 fallback for
    /// an application that sets only the old property.
    fn wm_title(&self, window: Window) -> Result<Option<String>, CuaError> {
        if self.net_wm_name != NONE && self.utf8_string != NONE {
            if let Some(bytes) = self.prop(window, self.net_wm_name, self.utf8_string)? {
                if let Some(text) = prop_text(&bytes) {
                    return Ok(Some(text));
                }
            }
        }
        match self.prop(window, AtomEnum::WM_NAME.into(), AtomEnum::STRING.into())? {
            Some(bytes) => Ok(prop_text(&bytes)),
            None => Ok(None),
        }
    }

    fn parent_of(&self, window: Window) -> Result<Option<Window>, CuaError> {
        let cookie = self.conn.query_tree(window).map_err(x_err)?;
        match cookie.reply() {
            Ok(reply) => Ok(Some(reply.parent)),
            Err(ReplyError::X11Error(_)) => Ok(None),
            Err(err) => Err(x_err(err)),
        }
    }

    /// Name a window the way the desktop names it.
    ///
    /// `WM_CLASS` and the title live on the *client* window, and neither the
    /// window under the pointer nor the one holding focus is usually that
    /// one: a reparenting window manager (xfwm4) puts a frame above it and
    /// Chromium puts render widgets below it, and neither carries the
    /// properties. Walking up to the first ancestor that names itself is
    /// what `xdotool getwindowclassname` does for the same reason.
    fn look(&self, window: Window) -> Result<WindowRef, CuaError> {
        let mut probe = window;
        for _ in 0..MAX_WINDOW_DEPTH {
            if let Some(class) = self.wm_class(probe)? {
                return Ok(WindowRef {
                    id: window_id(probe),
                    class: Some(class),
                    title: self.wm_title(probe)?,
                });
            }
            let Some(parent) = self.parent_of(probe)? else {
                break;
            };
            if parent == NONE || parent == self.root {
                break;
            }
            probe = parent;
        }
        // Nothing in the ancestry claimed a class. Report the window that was
        // actually there rather than inventing one, and still try for a title.
        Ok(WindowRef {
            id: window_id(window),
            class: None,
            title: self.wm_title(window)?,
        })
    }

    /// The window covering a screen coordinate, without moving the pointer.
    ///
    /// `TranslateCoordinates` is a pure query, so this answers what the click
    /// is about to land on rather than what it left behind — a click that
    /// raises or maps a window has already changed the answer by the time it
    /// returns.
    pub(crate) fn window_at(&self, x: i32, y: i32) -> Result<Option<WindowRef>, CuaError> {
        let (x, y) = xy(x, y)?;
        let mut deepest = self.root;
        for _ in 0..MAX_WINDOW_DEPTH {
            let cookie = self
                .conn
                .translate_coordinates(self.root, deepest, x, y)
                .map_err(x_err)?;
            let child = match cookie.reply() {
                Ok(reply) => reply.child,
                // The window went away mid-walk. What the box has so far is
                // still the truth about where the pointer is going.
                Err(ReplyError::X11Error(_)) => break,
                Err(err) => return Err(x_err(err)),
            };
            if child == NONE {
                break;
            }
            deepest = child;
        }
        if deepest == self.root {
            // Bare root: nothing is mapped at that coordinate. Absent, not
            // an empty window.
            return Ok(None);
        }
        self.look(deepest).map(Some)
    }

    /// Where a keystroke sent right now would be delivered.
    pub(crate) fn focus(&self) -> Result<Focus, CuaError> {
        let reply = self
            .conn
            .get_input_focus()
            .map_err(x_err)?
            .reply()
            .map_err(x_err)?;
        match reply.focus {
            FOCUS_NONE => Ok(Focus {
                state: FocusState::None,
                window: None,
            }),
            FOCUS_POINTER_ROOT => Ok(Focus {
                state: FocusState::PointerRoot,
                window: self.pointer_window()?,
            }),
            window if window == self.root => Ok(Focus {
                state: FocusState::Root,
                window: None,
            }),
            window => Ok(Focus {
                state: FocusState::Window,
                window: Some(self.look(window)?),
            }),
        }
    }

    fn pointer_window(&self) -> Result<Option<WindowRef>, CuaError> {
        let reply = self
            .conn
            .query_pointer(self.root)
            .map_err(x_err)?
            .reply()
            .map_err(x_err)?;
        self.window_at(i32::from(reply.root_x), i32::from(reply.root_y))
    }

    /// `(keycode, need_shift)` for a keysym in the current map.
    fn lookup(&self, keysym: Keysym) -> Option<(Keycode, bool)> {
        let per = self.keysyms_per as usize;
        if per == 0 {
            return None;
        }
        for (i, chunk) in self.keysyms.chunks(per).enumerate() {
            if chunk.first().copied() == Some(keysym) {
                return Some((self.min_keycode.saturating_add(i as u8), false));
            }
            if per > 1 && chunk.get(1).copied() == Some(keysym) {
                return Some((self.min_keycode.saturating_add(i as u8), true));
            }
        }
        None
    }

    fn fake(&self, ty: u8, detail: u8, x: i16, y: i16) -> Result<(), CuaError> {
        self.conn
            .xtest_fake_input(ty, detail, 0, NONE, x, y, 0)
            .map_err(x_err)?;
        Ok(())
    }

    fn flush(&self) -> Result<(), CuaError> {
        self.conn.flush().map_err(x_err)
    }

    fn sync(&self) -> Result<(), CuaError> {
        self.flush()?;
        let _ = self
            .conn
            .get_input_focus()
            .map_err(x_err)?
            .reply()
            .map_err(x_err)?;
        Ok(())
    }

    fn key(&self, keycode: Keycode, press: bool) -> Result<(), CuaError> {
        if keycode == 0 {
            return Err(x_err("missing keycode"));
        }
        let ty = if press {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        self.fake(ty, keycode, 0, 0)
    }

    pub(crate) fn motion(&self, x: i32, y: i32) -> Result<(), CuaError> {
        let (x, y) = xy(x, y)?;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x, y)?;
        self.flush()
    }

    pub(crate) fn pointer_press(&self, x: i32, y: i32, button: u8) -> Result<(), CuaError> {
        let (x, y) = xy(x, y)?;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x, y)?;
        self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0)?;
        self.flush()
    }

    pub(crate) fn button_up(&self, button: u8) -> Result<(), CuaError> {
        self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0)?;
        self.flush()
    }

    pub(crate) fn button_click(&self, button: u8) -> Result<(), CuaError> {
        self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0)?;
        self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0)?;
        self.flush()
    }

    pub(crate) fn motion_path_and_release(
        &self,
        points: &[(i32, i32)],
        button: u8,
    ) -> Result<(), CuaError> {
        for &(x, y) in points {
            let (x, y) = xy(x, y)?;
            self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x, y)?;
        }
        self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0)?;
        self.flush()
    }

    pub(crate) fn scroll(&self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), CuaError> {
        let (x, y) = xy(x, y)?;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x, y)?;
        fn wheel(conn: &InputConn, button: u8, ticks: u32) -> Result<(), CuaError> {
            for _ in 0..ticks {
                conn.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0)?;
                conn.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0)?;
            }
            Ok(())
        }
        if dy != 0 {
            let button = if dy > 0 { 5 } else { 4 };
            wheel(self, button, crate::wheel_ticks(dy))?;
        }
        if dx != 0 {
            let button = if dx > 0 { 7 } else { 6 };
            wheel(self, button, crate::wheel_ticks(dx))?;
        }
        self.flush()
    }

    fn release_mask(&self, mask: u16) -> Result<ArrayVec<Keycode, 8>, CuaError> {
        let mut released = ArrayVec::new();
        let per = self.keycodes_per_mod as usize;
        if per == 0 {
            return Ok(released);
        }
        // X modifier order: Shift, Lock, Control, Mod1..Mod5.
        for mod_index in 0..8 {
            if mask & (1 << mod_index) == 0 {
                continue;
            }
            let start = mod_index * per;
            let Some(&kc) = self
                .mod_keycodes
                .get(start..start + per)
                .and_then(|s| s.iter().find(|c| **c != 0))
            else {
                continue;
            };
            self.key(kc, false)?;
            // SAFETY: at most 8 modifiers, matching ArrayVec cap.
            unsafe { released.push_unchecked(kc) };
        }
        Ok(released)
    }

    fn query_mod_mask(&self) -> Result<u16, CuaError> {
        let reply = self
            .conn
            .query_pointer(self.root)
            .map_err(x_err)?
            .reply()
            .map_err(x_err)?;
        Ok(key_but_mask_bits(reply.mask))
    }

    fn send_keysym(&mut self, keysym: Keysym, action: KeyAction) -> Result<(), CuaError> {
        if let Some((kc, shift)) = self.lookup(keysym) {
            match action {
                KeyAction::Tap => {
                    if shift {
                        self.key(self.shift_l, true)?;
                    }
                    self.key(kc, true)?;
                    self.key(kc, false)?;
                    if shift {
                        self.key(self.shift_l, false)?;
                    }
                }
                KeyAction::Down => {
                    if shift {
                        self.key(self.shift_l, true)?;
                    }
                    self.key(kc, true)?;
                }
                KeyAction::Up => {
                    self.key(kc, false)?;
                    if shift {
                        self.key(self.shift_l, false)?;
                    }
                }
            }
            return Ok(());
        }
        self.send_via_scratch(keysym, action)
    }

    fn send_via_scratch(&mut self, keysym: Keysym, action: KeyAction) -> Result<(), CuaError> {
        let per = self.keysyms_per.max(1);
        let old = self
            .conn
            .get_keyboard_mapping(self.scratch, 1)
            .map_err(x_err)?
            .reply()
            .map_err(x_err)?;
        let new = SmallVec::<[Keysym; 8]>::from_elem(keysym, per as usize);
        self.conn
            .change_keyboard_mapping(1, self.scratch, per, &new)
            .map_err(x_err)?;
        self.sync()?;
        let _guard = MappingGuard {
            conn: &self.conn,
            keycode: self.scratch,
            per,
            old: old.keysyms,
        };
        match action {
            KeyAction::Tap => {
                self.key(self.scratch, true)?;
                self.key(self.scratch, false)?;
            }
            KeyAction::Down => self.key(self.scratch, true)?,
            KeyAction::Up => self.key(self.scratch, false)?,
        }
        self.flush()?;
        Ok(())
    }

    /// Keysyms, not text: `char_to_keysym` is pure, so the caller resolves
    /// them before the connection is taken and the deadline only has to
    /// cover X round-trips.
    pub(crate) fn type_keysyms(&mut self, keysyms: &[Keysym]) -> Result<(), CuaError> {
        let mask = self.query_mod_mask()?;
        let released = self.release_mask(mask)?;
        for &keysym in keysyms {
            self.send_keysym(keysym, KeyAction::Tap)?;
        }
        for kc in released {
            self.key(kc, true)?;
        }
        self.flush()
    }

    pub(crate) fn key_seq(
        &mut self,
        seq: &[Keysym],
        action: KeyAction,
        clear: bool,
    ) -> Result<(), CuaError> {
        let released = if clear {
            let mask = self.query_mod_mask()?;
            self.release_mask(mask)?
        } else {
            ArrayVec::new()
        };
        match action {
            KeyAction::Tap => {
                for ks in seq {
                    self.send_keysym(*ks, KeyAction::Down)?;
                }
                for ks in seq.iter().rev() {
                    self.send_keysym(*ks, KeyAction::Up)?;
                }
            }
            KeyAction::Down => {
                for ks in seq {
                    self.send_keysym(*ks, KeyAction::Down)?;
                }
            }
            KeyAction::Up => {
                for ks in seq.iter().rev() {
                    self.send_keysym(*ks, KeyAction::Up)?;
                }
            }
        }
        if clear {
            for kc in released {
                self.key(kc, true)?;
            }
        }
        self.flush()
    }
}

fn xy(x: i32, y: i32) -> Result<(i16, i16), CuaError> {
    Ok((
        i16::try_from(x).map_err(|_| CuaError::Invalid("x does not fit i16".into()))?,
        i16::try_from(y).map_err(|_| CuaError::Invalid("y does not fit i16".into()))?,
    ))
}

fn key_but_mask_bits(mask: KeyButMask) -> u16 {
    u16::from(mask)
}

/// Decide what a finished XTEST attempt means for the caller.
///
/// A protocol failure or a missing XTEST extension is worth retrying through
/// xdotool. A wedged server is not: xdotool drives the same X server, so the
/// fallback would burn its own 2.5 s timeout and then report a tool failure
/// where the truth is "the display stopped answering".
async fn or_xdotool<F, Fut>(
    outcome: Native<()>,
    op: &'static str,
    fallback: F,
) -> Result<(), CuaError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), CuaError>>,
{
    match outcome {
        Native::Ran(Ok(())) => Ok(()),
        Native::Ran(Err(err)) => {
            tracing::warn!(op, error = %err, "xtest failed; xdotool");
            fallback().await
        }
        Native::Unavailable => fallback().await,
        Native::Wedged(err) => Err(err),
    }
}

/// Look, and never let looking fail a step.
///
/// Observation has no caller waiting on its result the way an actuator does,
/// so every way it can go wrong collapses to the same answer: the receipt
/// does not claim the fact. The connection is a parameter for the same
/// reason `with_native_on` takes one — so a test can wedge one of its own.
async fn observed<T: Send + 'static>(
    observe: &'static XInput,
    config: &CuaConfig,
    op: impl FnOnce(&mut InputConn) -> Result<Option<T>, CuaError> + Send + 'static,
) -> Option<T> {
    match with_native_on(observe, &config.display, OBSERVE_DEADLINE, op).await {
        Native::Ran(Ok(found)) => found,
        Native::Ran(Err(err)) => {
            tracing::debug!(error = %err, "observation failed; the receipt will not claim it");
            None
        }
        Native::Unavailable => None,
        Native::Wedged(err) => {
            tracing::warn!(error = %err, "observation gave up on the X server");
            None
        }
    }
}

pub(crate) async fn observe_window_at(config: &CuaConfig, x: i32, y: i32) -> Option<WindowRef> {
    observed(&OBSERVE, config, move |c| c.window_at(x, y)).await
}

pub(crate) async fn observe_focus(config: &CuaConfig) -> Option<Focus> {
    observed(&OBSERVE, config, |c| c.focus().map(Some)).await
}

pub(crate) async fn move_pointer(config: &CuaConfig, x: i32, y: i32) -> Result<(), CuaError> {
    let outcome = with_native(&config.display, XTEST_DEADLINE, move |c| c.motion(x, y)).await;
    or_xdotool(outcome, "motion", || async move {
        crate::xdotool::xdotool(config, &["mousemove", &x.to_string(), &y.to_string()]).await
    })
    .await
}

pub(crate) async fn pointer_press(
    config: &CuaConfig,
    x: i32,
    y: i32,
    button: u8,
) -> Result<(), CuaError> {
    let outcome = with_native(&config.display, XTEST_DEADLINE, move |c| {
        c.pointer_press(x, y, button)
    })
    .await;
    or_xdotool(outcome, "press", || async move {
        crate::xdotool::xdotool_owned(config, &crate::pointer_press_args(x, y, button)).await
    })
    .await
}

pub(crate) async fn button_up(config: &CuaConfig, button: u8) -> Result<(), CuaError> {
    let outcome = with_native(&config.display, XTEST_DEADLINE, move |c| {
        c.button_up(button)
    })
    .await;
    or_xdotool(outcome, "button-up", || async move {
        crate::xdotool::xdotool(config, &["mouseup", &button.to_string()]).await
    })
    .await
}

pub(crate) async fn button_click(config: &CuaConfig, button: u8) -> Result<(), CuaError> {
    let outcome = with_native(&config.display, XTEST_DEADLINE, move |c| {
        c.button_click(button)
    })
    .await;
    or_xdotool(outcome, "click", || async move {
        crate::xdotool::xdotool(
            config,
            &[
                "mousedown",
                &button.to_string(),
                "mouseup",
                &button.to_string(),
            ],
        )
        .await
    })
    .await
}

pub(crate) async fn motion_path_and_release(
    config: &CuaConfig,
    points: &[(i32, i32)],
    extra: Option<(i32, i32)>,
    path: Option<&[CuaPoint]>,
    button: u8,
) -> Result<(), CuaError> {
    let mut all = ArrayVec::<(i32, i32), MAX_COLLECTED_POINTS>::new();
    for &pt in points {
        all.try_push(pt)
            .map_err(|_| CuaError::Invalid("too many motion points".into()))?;
    }
    if let Some(path) = path {
        for p in path {
            all.try_push((p.x, p.y))
                .map_err(|_| CuaError::Invalid("too many motion points".into()))?;
        }
    }
    if let Some(xy) = extra {
        all.try_push(xy)
            .map_err(|_| CuaError::Invalid("too many motion points".into()))?;
    }
    match with_native(&config.display, XTEST_DEADLINE, move |c| {
        c.motion_path_and_release(&all, button)
    })
    .await
    {
        Native::Ran(Ok(())) => Ok(()),
        // The caller (drag / release) owns the xdotool retry here, because
        // only it knows the waypoints. A wedge is still not retryable.
        Native::Ran(Err(err)) => {
            tracing::warn!(error = %err, "xtest release failed; xdotool");
            Err(err)
        }
        Native::Unavailable => Err(CuaError::Tool("xtest unavailable".into())),
        Native::Wedged(err) => Err(err),
    }
}

pub(crate) async fn scroll(
    config: &CuaConfig,
    x: i32,
    y: i32,
    dx: i32,
    dy: i32,
) -> Result<(), CuaError> {
    let outcome = with_native(&config.display, XTEST_DEADLINE, move |c| {
        c.scroll(x, y, dx, dy)
    })
    .await;
    or_xdotool(outcome, "scroll", || async move {
        crate::xdotool::xdotool_owned(config, &crate::scroll_args(x, y, dx, dy)).await
    })
    .await
}

pub(crate) async fn type_text(config: &CuaConfig, text: &str) -> Result<(), CuaError> {
    let keysyms: SmallVec<[Keysym; 8]> = text.chars().map(char_to_keysym).collect();
    // One call types the whole string, so the budget has to grow with it:
    // an unmapped keysym is a keymap round-trip per character. See
    // `TYPE_CHAR_BUDGET`.
    let budget = XTEST_DEADLINE + TYPE_CHAR_BUDGET * keysyms.len() as u32;
    let outcome = with_native(&config.display, budget, move |c| c.type_keysyms(&keysyms)).await;
    or_xdotool(outcome, "type", || async move {
        crate::xdotool::xdotool(
            config,
            &["type", "--clearmodifiers", "--delay", "1", "--", text],
        )
        .await
    })
    .await
}

pub(crate) async fn key(config: &CuaConfig, key: &str, action: KeyAction) -> Result<(), CuaError> {
    let clear = matches!(action, KeyAction::Tap);
    // Parsed before the blocking hop because it is pure. A name this crate
    // does not know is not an invalid key: xdotool knows keysym names (the
    // XF86 media keys) that `keys.rs` does not, and that fallback predates
    // the deadline work.
    let Ok(seq) = parse_key_sequence(key) else {
        tracing::warn!(key, "key name is not in the keysym table; xdotool");
        return xdotool_key(config, key, action).await;
    };
    let outcome = with_native(&config.display, XTEST_DEADLINE, move |c| {
        c.key_seq(&seq, action, clear)
    })
    .await;
    or_xdotool(outcome, "key", || async move {
        xdotool_key(config, key, action).await
    })
    .await
}

async fn xdotool_key(config: &CuaConfig, key: &str, action: KeyAction) -> Result<(), CuaError> {
    match action {
        KeyAction::Tap => crate::xdotool::xdotool(config, &["key", "--clearmodifiers", key]).await,
        KeyAction::Down => crate::xdotool::xdotool(config, &["keydown", key]).await,
        KeyAction::Up => crate::xdotool::xdotool(config, &["keyup", key]).await,
    }
}

/// Shapes the box turns X replies into. No X server required.
#[cfg(test)]
mod shapes {
    use super::*;

    #[test]
    fn wm_class_reads_as_wmctrl_prints_it() {
        assert_eq!(
            format_wm_class(b"chromium\0Chromium\0").as_deref(),
            Some("chromium.Chromium")
        );
        // The same spelling `GET /v1/desktop/windows` reports, so a receipt
        // and a window list can be compared without translating one.
        assert_eq!(
            format_wm_class(b"xfce4-panel\0Xfce4-panel\0").as_deref(),
            Some("xfce4-panel.Xfce4-panel")
        );
        // A toolkit that sets only the instance still names itself.
        assert_eq!(format_wm_class(b"xterm\0").as_deref(), Some("xterm"));
        assert_eq!(format_wm_class(b"").as_deref(), None);
        assert_eq!(format_wm_class(b"\0\0").as_deref(), None);
    }

    #[test]
    fn window_ids_match_the_form_wmctrl_prints() {
        assert_eq!(window_id(0x02a0_0003), "0x02a00003");
        assert_eq!(window_id(0), "0x00000000");
    }

    #[test]
    fn a_blank_title_is_absent_not_empty() {
        assert_eq!(
            prop_text(b"New Tab - Chromium\0").as_deref(),
            Some("New Tab - Chromium")
        );
        assert_eq!(prop_text(b"\0").as_deref(), None);
        assert_eq!(prop_text(b"   ").as_deref(), None);
        assert_eq!(prop_text(b"").as_deref(), None);
    }
}

#[cfg(test)]
mod live {
    use super::*;

    fn live_display() -> Option<String> {
        let display = std::env::var("DISPLAY").ok().filter(|d| !d.is_empty())?;
        let num = display
            .trim()
            .trim_start_matches(':')
            .split('.')
            .next()
            .unwrap_or("1");
        if std::path::Path::new("/tmp/.X11-unix")
            .join(format!("X{num}"))
            .exists()
        {
            Some(display)
        } else {
            None
        }
    }

    #[test]
    fn getimage_png_when_display_is_up() {
        let Some(display) = live_display() else {
            return;
        };
        let png = capture_png(&display, 1280, 800).expect("GetImage PNG");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(png.len() > 64, "png too small: {}", png.len());
    }

    #[tokio::test]
    async fn xtest_motion_when_display_is_up() {
        let Some(display) = live_display() else {
            return;
        };
        match with_native(&display, XTEST_DEADLINE, |c| c.motion(16, 16)).await {
            Native::Ran(result) => result.expect("motion"),
            Native::Unavailable => panic!("XTEST should be present"),
            Native::Wedged(err) => panic!("live display should answer: {err}"),
        }
    }

    /// Needs a real X server, so it skips everywhere the two above do —
    /// including CI, which starts no display.
    #[tokio::test]
    async fn observation_names_the_desktop_it_is_looking_at() {
        let Some(display) = live_display() else {
            return;
        };
        let mut config = CuaConfig::disabled();
        config.enabled = true;
        config.display = display;

        // Whatever is at the centre of the screen, the box must either name a
        // window or say nothing — never an id with no window behind it.
        if let Some(found) = observe_window_at(&config, 16, 16).await {
            assert!(
                found.id.starts_with("0x"),
                "window ids are wmctrl-shaped: {found:?}"
            );
        }

        let focus = observe_focus(&config)
            .await
            .expect("a live server always answers GetInputFocus");
        // The one thing that must hold: a state naming a window comes with
        // one, and a state naming nowhere does not invent one.
        match focus.state {
            FocusState::Window => assert!(focus.window.is_some(), "{focus:?}"),
            FocusState::None | FocusState::Root => assert!(focus.window.is_none(), "{focus:?}"),
            FocusState::PointerRoot => {}
        }
    }
}

/// A paused X server, without an X server.
#[cfg(test)]
mod deadlines {
    use super::*;

    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;

    /// Wedging the process-wide `INPUT` would park it for the rest of the
    /// test binary, so this module wedges a connection of its own.
    static TEST_INPUT: XInput = XInput::const_new(None);

    /// And a second one, so wedging the observation path does not wedge the
    /// input path this module is really about.
    static TEST_OBSERVE: XInput = XInput::const_new(None);

    struct FakeServer {
        display: String,
        path: std::path::PathBuf,
        release: mpsc::Sender<()>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeServer {
        /// Accepts, then says nothing: from the client side this is exactly a
        /// `kill -STOP`ped X server, because x11rb blocks in the setup
        /// handshake and has no reply deadline.
        ///
        /// `None` when the socket cannot be placed, which is the same skip
        /// the live tests above take when there is nothing to talk to.
        fn wedged() -> Option<Self> {
            let dir = std::path::Path::new("/tmp/.X11-unix");
            std::fs::create_dir_all(dir).ok()?;
            // x11rb builds the socket path from the display number, so the
            // fake server has to sit where a real one would. Bind decides
            // which number is free: an occupied path is somebody else's X
            // server and must not be unlinked.
            let (display, path, listener) = (900..=920).find_map(|n| {
                let path = dir.join(format!("X{n}"));
                let listener = UnixListener::bind(&path).ok()?;
                Some((format!(":{n}"), path, listener))
            })?;
            let (release, wait) = mpsc::channel();
            let thread = std::thread::spawn(move || {
                let held = listener.accept().map(|(stream, _)| stream);
                let _ = wait.recv();
                drop(held);
            });
            Some(Self {
                display,
                path,
                release,
                thread: Some(thread),
            })
        }

        fn display(&self) -> String {
            self.display.clone()
        }

        /// Hang up, which unparks the abandoned worker.
        fn resume(&mut self) {
            let _ = self.release.send(());
            // If an assertion fired before anything connected, the thread is
            // still in `accept`; knock so that joining it cannot hang the
            // test the way this whole file is about not hanging.
            let _ = std::os::unix::net::UnixStream::connect(&self.path);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            self.resume();
            let _ = std::fs::remove_file(&self.path);
        }
    }

    async fn motion(display: &str, budget: Duration) -> Native<()> {
        tokio::time::timeout(
            Duration::from_secs(10),
            with_native_on(&TEST_INPUT, display, budget, |c| c.motion(1, 1)),
        )
        .await
        .expect("with_native must return on its own deadline, never hang")
    }

    #[tokio::test]
    async fn wedged_server_times_out_and_later_calls_do_not_queue() {
        let Some(mut server) = FakeServer::wedged() else {
            eprintln!("no writable /tmp/.X11-unix slot; skipping");
            return;
        };
        let display = server.display();
        let budget = Duration::from_millis(300);

        let started = Instant::now();
        let first = motion(&display, budget).await;
        let waited = started.elapsed();
        match first {
            Native::Wedged(CuaError::DisplayDown(msg)) => {
                assert!(msg.contains("timed out"), "reported as a timeout: {msg}");
            }
            Native::Ran(Ok(())) => panic!("a silent server cannot have moved the pointer"),
            Native::Ran(Err(err)) => panic!("a silent server is not a failure: {err}"),
            Native::Unavailable => panic!("connecting to a silent server cannot finish"),
            Native::Wedged(err) => panic!("wedges are 503 DisplayDown, got {err:?}"),
        }
        assert!(waited >= budget, "returned before the deadline: {waited:?}");
        assert!(waited < budget * 4, "far past the deadline: {waited:?}");

        // The worker is still parked in the X server holding the connection.
        // The next caller must be told that, not queued behind it.
        let started = Instant::now();
        match motion(&display, budget).await {
            Native::Wedged(CuaError::DisplayDown(msg)) => {
                assert!(msg.contains("busy"), "reported as busy: {msg}");
            }
            other => panic!("second call should report the wedge: {}", name(&other)),
        }
        assert!(
            started.elapsed() < INPUT_LOCK_WAIT * 4,
            "second call waited for the whole X deadline"
        );

        // Recovery without restarting the process: the parked worker returns,
        // drops the guard, and the connection is usable again. (This fake
        // server hangs up rather than answering, so the backend it settles on
        // is the xdotool fallback.)
        server.resume();
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Native::Wedged(err) = motion(&display, budget).await {
            assert!(Instant::now() < deadline, "never recovered: {err}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A silent X server must cost the receipt a fact, not the recipe a step.
    #[tokio::test]
    async fn a_wedged_server_is_not_observed_and_is_not_an_error() {
        let Some(server) = FakeServer::wedged() else {
            eprintln!("no writable /tmp/.X11-unix slot; skipping");
            return;
        };
        let mut config = CuaConfig::disabled();
        config.enabled = true;
        config.display = server.display();

        let started = Instant::now();
        let seen = tokio::time::timeout(
            Duration::from_secs(10),
            observed(&TEST_OBSERVE, &config, |c| c.window_at(100, 100)),
        )
        .await
        .expect("an observation must return on its own deadline, never hang");

        // Absent, which the receipt renders as "not observed". The step that
        // follows still runs; nothing about the recipe failed because the box
        // could not see.
        assert!(
            seen.is_none(),
            "a server that never answered cannot have named a window: {seen:?}"
        );
        assert!(
            started.elapsed() < OBSERVE_DEADLINE * 4,
            "observation ran past its own deadline: {:?}",
            started.elapsed()
        );
    }

    fn name<T>(outcome: &Native<T>) -> &'static str {
        match outcome {
            Native::Ran(Ok(_)) => "ran ok",
            Native::Ran(Err(_)) => "ran with an error",
            Native::Unavailable => "unavailable",
            Native::Wedged(_) => "wedged",
        }
    }
}
