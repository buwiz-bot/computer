//! The latest tree read of each screen, so a `ref` from it can be acted on. Only the latest: a ref
//! from an older tree is stale, and is refused rather than guessed at.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tree::Handle;
use crate::A11yError;

#[derive(Clone, Debug)]
pub struct Snapshots(Arc<Mutex<Store>>);

#[derive(Debug)]
struct Store {
    last: u64,
    screens: HashMap<String, Taken>,
}

#[derive(Debug)]
struct Taken {
    id: u64,
    handles: Vec<Handle>,
}

impl Default for Snapshots {
    fn default() -> Self {
        // Numbers start somewhere new each time box-exec does, so a ref kept from before a restart
        // is very unlikely to name a node of a new tree.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let seed = (now.subsec_nanos() as u64 ^ ((std::process::id() as u64) << 7)) % 900_000;
        Self::starting_at(seed + 1000)
    }
}

impl Snapshots {
    /// A store whose first snapshot is `first + 1`.
    pub fn starting_at(first: u64) -> Self {
        Self(Arc::new(Mutex::new(Store {
            last: first,
            screens: HashMap::new(),
        })))
    }

    /// Keeps `handles` as `display`'s latest tree, in place of the one before, and numbers it.
    pub fn record(&self, display: &str, handles: Vec<Handle>) -> u64 {
        let mut store = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        store.last += 1;
        let id = store.last;
        store
            .screens
            .insert(display.to_string(), Taken { id, handles });
        id
    }

    /// The node `reference` names in `display`'s latest tree.
    pub fn resolve(&self, display: &str, reference: &str) -> Result<Handle, A11yError> {
        let (id, index) = parse_ref(reference)?;
        let store = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let Some(taken) = store.screens.get(display) else {
            return Err(A11yError::Stale(format!(
                "no tree of {display} has been read since box-exec started: read one with POST /v1/cua/tree"
            )));
        };
        if taken.id != id {
            return Err(A11yError::Stale(format!(
                "ref {reference} is from tree {id}, and {display}'s latest is {}: read the tree again",
                taken.id
            )));
        }
        taken
            .handles
            .get(index)
            .cloned()
            .ok_or_else(|| A11yError::Invalid(format!("tree {id} has no node {index}")))
    }
}

fn parse_ref(reference: &str) -> Result<(u64, usize), A11yError> {
    let invalid = || {
        A11yError::Invalid(format!(
            "a ref is <tree>:<node>, as POST /v1/cua/tree gives it, not {:?}",
            crate::tree::clip(reference, 40)
        ))
    };
    let (id, index) = reference.trim().split_once(':').ok_or_else(invalid)?;
    let digits = |s: &str| !s.is_empty() && s.len() <= 19 && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(id) || !digits(index) {
        return Err(invalid());
    }
    Ok((
        id.parse().map_err(|_| invalid())?,
        index.parse().map_err(|_| invalid())?,
    ))
}
