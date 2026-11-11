//! `box-egress-tunnel`: the guest's HTTP CONNECT proxy for Chromium, which judges every request by
//! the computer's rules and then dials the site itself (direct exit) or muxes it over a WebSocket
//! to a client on the operator's machine (tunnel exit).
//!
//! This is **our** wire (`box-egress-v1`). It is not Cursor's
//! `sand-egress-tunnel` and does not re-host any proprietary binary.
//!
//! Layout:
//! - **server** (inside the guest): the CONNECT proxy for Chromium, the WebSocket for the
//!   operator's client when the tunnel is on, and a loopback admin port for status and rule
//!   reloads.
//! - the wire, the rules, the address policy and the **client** (on the operator's machine) live
//!   in the `box-egress` crate, re-exported here, so the operator's side can use them without this
//!   crate's daemon.
//!
//! See `docs/EGRESS.md` in the repo root for the operator story and the
//! byte-level protocol.

pub use box_egress::{allowlist, client, destination, dial, protocol, rules};

pub mod config;
pub mod http1;
pub mod server;
pub mod status;

pub use box_egress::{Allowlist, TunnelClientConfig};
pub use config::{TunnelServerConfig, WsSide};
pub use protocol::{PROTOCOL_NAME, WIRE_VERSION};
pub use status::{probe_egress, Counts, EgressConfig, EgressStatus};

#[cfg(test)]
mod relay_tests;
