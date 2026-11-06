//! Who a request's work runs as. `box-exec` is a service, and nothing a caller asks for runs as it:
//! a command, or a file touched for a Bot, runs as that Bot's own user, and a request that names no
//! Bot as the shared `agent` user. A Bot this computer hasn't got is refused.
//!
//! `bot` is the caller's word. That's a wall against commands, which can't call this: they have no
//! token, and the broker, the only holder of one, names the Bot from its own records. Anyone with
//! the token could name any Bot.
//!
//! Only root writes `bots` (`box-screen`), so a Bot's number here is the one it was given. In the
//! box `box-exec` holds `CAP_SETUID`, `CAP_SETGID`, `CAP_KILL` and `CAP_NET_BIND_SERVICE`, kept
//! here and passed to nothing ([`keep_capabilities_here`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use box_common::{valid_bot, ApiError};

const BOTS_DIR: &str = "/home/box/.box/bots";
const SHARED_UID: u32 = 1500;
const SHARED_HOME: &str = "/home/box/agents/shared";
const FIRST_USER: u32 = 20000;

/// A user a request's work runs as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

impl Identity {
    /// All of a command's environment that doesn't come from its request.
    pub fn env(&self) -> HashMap<&'static str, String> {
        let home = self.home.display().to_string();
        HashMap::from([
            ("PATH", "/usr/local/bin:/usr/bin:/bin".to_string()),
            ("LANG", "C.UTF-8".to_string()),
            ("USER", format!("u{}", self.uid)),
            ("TMPDIR", "/tmp".to_string()),
            ("WORKSPACE_ROOT", "/workspace".to_string()),
            ("HOME", home),
        ])
    }
}

#[derive(Clone, Debug)]
pub enum Users {
    /// Outside the box: everything runs as whoever started this.
    Same,
    /// In the box, with the Bots' numbers in this folder.
    Agents(PathBuf),
}

impl Users {
    pub fn from_env() -> Self {
        match std::env::var("BOX_RUN_AS").as_deref() {
            Ok("agents") => Self::Agents(PathBuf::from(BOTS_DIR)),
            _ => Self::Same,
        }
    }

    /// The user `bot`'s work runs as, or the shared one's for none. `None` is only outside the box.
    pub fn who(&self, bot: Option<&str>) -> Result<Option<Identity>, ApiError> {
        let Self::Agents(bots) = self else {
            return Ok(None);
        };
        let Some(bot) = bot else {
            return Ok(Some(Identity {
                uid: SHARED_UID,
                gid: SHARED_UID,
                home: PathBuf::from(SHARED_HOME),
            }));
        };
        identity_of(bots, bot).map(Some)
    }
}

fn identity_of(bots: &Path, bot: &str) -> Result<Identity, ApiError> {
    if !valid_bot(bot) {
        return Err(ApiError::invalid_request(
            "a Bot id is letters, digits, _ and - only",
        ));
    }
    let unknown = || ApiError::not_found(format!("no Bot {bot} on this computer"));
    let index: u32 = std::fs::read_to_string(bots.join(bot))
        .map_err(|_| unknown())?
        .trim()
        .parse()
        .map_err(|_| unknown())?;
    let uid = index
        .checked_mul(2)
        .and_then(|n| n.checked_add(FIRST_USER))
        .ok_or_else(unknown)?;
    Ok(Identity {
        uid,
        gid: uid,
        home: PathBuf::from(format!("/home/box/agents/{bot}")),
    })
}

/// Runs `work` under `who`'s file identity, on a thread of its own: `setfsuid` is per thread, and a
/// pool thread would keep it. `None` runs it here.
pub fn as_user<T: Send>(
    who: Option<&Identity>,
    work: impl FnOnce() -> T + Send,
) -> Result<T, ApiError> {
    let Some(who) = who else {
        return Ok(work());
    };
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                become_for_files(who)?;
                Ok(work())
            })
            .join()
            .unwrap_or_else(|_| Err(ApiError::internal("file work panicked")))
    })
}

/// [`as_user`] off the async workers.
pub async fn on_disk<T: Send + 'static>(
    who: Option<Identity>,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(move || as_user(who.as_ref(), work))
        .await
        .map_err(|_| ApiError::internal("file work stopped"))?
}

#[cfg(target_os = "linux")]
fn become_for_files(who: &Identity) -> Result<(), ApiError> {
    // SAFETY: these take numbers and touch no memory. Neither reports failure except by not
    // changing, so each is read back: asking for an impossible id answers the current one.
    let became = unsafe {
        libc::setfsgid(who.gid);
        libc::setfsuid(who.uid);
        (
            libc::setfsgid(u32::MAX) as u32,
            libc::setfsuid(u32::MAX) as u32,
        )
    };
    if became == (who.gid, who.uid) {
        Ok(())
    } else {
        Err(ApiError::internal("couldn't take the user's file identity"))
    }
}

#[cfg(not(target_os = "linux"))]
fn become_for_files(_who: &Identity) -> Result<(), ApiError> {
    Err(ApiError::internal("users need Linux"))
}

/// What `box-keeper` gave this process stays here. Its capabilities came ambient, the only way
/// through an exec, so every child would have them too. With the ambient and inheritable sets
/// empty, a child's exec drops them all, as any other process's exec does. A box where this fails
/// doesn't serve.
#[cfg(target_os = "linux")]
pub fn keep_capabilities_here() -> std::io::Result<()> {
    const VERSION_3: u32 = 0x2008_0522;
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let mut header = Header {
        version: VERSION_3,
        pid: 0,
    };
    let mut data = [Data::default(); 2];
    // SAFETY: both point at live values of the layout the v3 calls take.
    unsafe {
        if libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        ) != 0
            || libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        data[0].inheritable = 0;
        data[1].inheritable = 0;
        if libc::syscall(libc::SYS_capset, &mut header, data.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn keep_capabilities_here() -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents(dir: &Path) -> Users {
        Users::Agents(dir.to_path_buf())
    }

    #[test]
    fn a_bot_runs_as_its_own_user_and_nobody_as_the_service() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("amy"), "3\n").unwrap();
        let users = agents(dir.path());
        let amy = users.who(Some("amy")).unwrap().unwrap();
        assert_eq!((amy.uid, amy.gid), (20006, 20006));
        assert_eq!(amy.home, Path::new("/home/box/agents/amy"));
        assert_eq!(users.who(None).unwrap().unwrap().uid, SHARED_UID);
    }

    #[test]
    fn a_bot_this_computer_hasnt_got_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken"), "not a number").unwrap();
        std::fs::write(dir.path().join("huge"), "4294967295").unwrap();
        let users = agents(dir.path());
        for bot in ["nobody", "broken", "huge", "../amy", "a b", ""] {
            assert!(users.who(Some(bot)).is_err(), "{bot:?}");
        }
    }

    #[test]
    fn a_commands_environment_is_only_its_own() {
        let who = Identity {
            uid: 20000,
            gid: 20000,
            home: PathBuf::from("/home/box/agents/amy"),
        };
        let env = who.env();
        assert_eq!(env["HOME"], "/home/box/agents/amy");
        assert!(!env.contains_key("XAUTHORITY") && !env.contains_key("DISPLAY"));
    }
}
