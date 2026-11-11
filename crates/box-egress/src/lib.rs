//! `box-egress`: the parts of `box-egress-v1` both ends share, and the operator-side client.
//!
//! - [`protocol`]: the wire (JSON control frames, binary data frames).
//! - [`rules`]: a computer's browsing rules: exit (direct or tunnel), mode, allow and block lists.
//! - [`destination`]: which resolved addresses and ports may be dialled at all.
//! - [`dial`]: resolve, judge, dial, with a DNS timeout and no second lookup.
//! - [`client`]: attach to a box's tunnel and dial for it, judging each stream by the rules again.
//! - [`allowlist`]: the operator's static name filter (`BOX_EGRESS_RELAY_HOSTS`).
//!
//! The guest's server (the proxy and the WebSocket) is in `box-egress-tunnel`, which builds on
//! this. Buwiz Bot's broker depends on this crate alone, for the client.

pub mod allowlist;
pub mod client;
pub mod destination;
pub mod dial;
pub mod protocol;
pub mod rules;

pub use allowlist::Allowlist;
pub use client::{ClientError, ClientSnapshot, ClientState, TunnelClientConfig};
pub use protocol::{PROTOCOL_NAME, WIRE_VERSION};
pub use rules::{Exit, Mode, Rules, RulesCell, RulesError, RulesSummary, Verdict};
