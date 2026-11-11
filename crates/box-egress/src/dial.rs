//! Resolve a name, judge every address it resolved to, and dial only an address that passed.
//! Used by whichever end dials: the operator's client in tunnel mode, the box's proxy directly.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;

use crate::destination::DestinationPolicy;

/// Longest a name may take to resolve. A resolver that never answers used to hold the stream until
/// the guest gave up on it at [`crate::protocol::OPEN_TIMEOUT`].
pub const DNS_TIMEOUT: Duration = Duration::from_secs(4);

/// Longest dialling a destination may take, over all of its vetted addresses. With
/// [`DNS_TIMEOUT`] it is below the guest's `OPEN_TIMEOUT`, so the guest hears why a stream failed
/// rather than giving up on it.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// The least time one address gets inside [`DIAL_TIMEOUT`], so a black-holed first address
/// leaves time for the next.
pub const ATTEMPT_FLOOR: Duration = Duration::from_secs(3);

/// Why a destination wasn't reached. The text never names the host: browsing hosts are private.
#[derive(Debug)]
pub enum DialError {
    /// The name didn't resolve, or not in time.
    Resolve,
    /// Every address it resolved to is refused (private, loopback, reserved, a port not relayed).
    Refused(&'static str),
    /// No vetted address answered.
    Connect(std::io::Error),
}

impl DialError {
    /// A short, stable word for logs, counts and the `X-Box-Egress-Reason` header.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Resolve => "resolve",
            Self::Refused(_) => "destination",
            Self::Connect(_) => "connect",
        }
    }

    pub fn timed_out(&self) -> bool {
        matches!(self, Self::Connect(err) if err.kind() == std::io::ErrorKind::TimedOut)
    }
}

impl std::fmt::Display for DialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resolve => f.write_str("resolve failed"),
            Self::Refused(_) => f.write_str("destination refused"),
            Self::Connect(err) => write!(f, "connect failed: {}", err.kind()),
        }
    }
}

/// Resolve `host`, keep the addresses `policy` allows, and connect to the first that answers.
/// Never resolves the name a second time: that is what lets a DNS rebind swap in an address just
/// refused.
pub async fn resolve_and_dial(
    host: &str,
    port: u16,
    policy: &DestinationPolicy,
) -> Result<TcpStream, DialError> {
    let lookup = tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host, port))).await;
    let resolved: Vec<SocketAddr> = match lookup {
        Ok(Ok(addrs)) => addrs.collect(),
        Ok(Err(_)) | Err(_) => return Err(DialError::Resolve),
    };
    dial(judge(resolved, policy)?, DIAL_TIMEOUT, ATTEMPT_FLOOR)
        .await
        .map_err(DialError::Connect)
}

/// The addresses a name resolved to that `policy` lets be dialled. None at all is a resolve
/// failure; only addresses that were all refused are a refusal.
fn judge(
    resolved: Vec<SocketAddr>,
    policy: &DestinationPolicy,
) -> Result<Vec<SocketAddr>, DialError> {
    let Some(first) = resolved.first() else {
        return Err(DialError::Resolve);
    };
    let first_refusal = policy.refuse_reason(first).unwrap_or("refused");
    let vetted = policy.vet(resolved);
    if vetted.is_empty() {
        return Err(DialError::Refused(first_refusal));
    }
    Ok(vetted)
}

/// Connect to the first of `addrs` that answers, all within `budget`. Each address gets its share
/// of `budget`, at least `floor`, and never more than what is left.
pub async fn dial(
    addrs: Vec<SocketAddr>,
    budget: Duration,
    floor: Duration,
) -> std::io::Result<TcpStream> {
    let timed_out = |after: Duration| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("connect timed out after {}ms", after.as_millis()),
        )
    };
    let deadline = tokio::time::Instant::now() + budget;
    let share = (budget / addrs.len().max(1) as u32).max(floor);
    let mut last = std::io::Error::other("no vetted address");
    for addr in addrs {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(timed_out(budget));
        }
        let cap = share.min(remaining);
        match tokio::time::timeout(cap, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(err)) => last = err,
            Err(_) => last = timed_out(cap),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_dead_address_falls_through_to_the_next() {
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_addr = closed.local_addr().unwrap();
        drop(closed);
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open_addr = open.local_addr().unwrap();
        let stream = dial(vec![closed_addr, open_addr], DIAL_TIMEOUT, ATTEMPT_FLOOR)
            .await
            .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), open_addr);
    }

    #[tokio::test]
    async fn a_destination_that_never_answers_stops_at_the_budget() {
        // TEST-NET-1: nothing routes there, so the connect either hangs or fails at once.
        let started = std::time::Instant::now();
        let dead = "192.0.2.1:9".parse().unwrap();
        let err = dial(
            vec![dead],
            Duration::from_millis(200),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "{err}");
    }

    #[tokio::test]
    async fn a_black_holed_first_address_leaves_time_for_the_next() {
        // TEST-NET-1 never answers; the live second address must still be reached in the budget.
        let dead = "192.0.2.1:9".parse().unwrap();
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open_addr = open.local_addr().unwrap();
        let started = std::time::Instant::now();
        let stream = dial(
            vec![dead, open_addr],
            Duration::from_millis(1000),
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), open_addr);
        assert!(started.elapsed() < Duration::from_millis(1000));
    }

    #[tokio::test]
    async fn a_name_that_resolves_to_loopback_is_refused_by_the_default_policy() {
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = open.local_addr().unwrap().port();
        let policy = DestinationPolicy::default().with_ports(Some(Vec::new()));
        let err = resolve_and_dial("127.0.0.1", port, &policy)
            .await
            .unwrap_err();
        assert!(matches!(err, DialError::Refused("loopback")), "{err:?}");
        assert_eq!(err.to_string(), "destination refused");
        let opted_in = policy.with_allow_private(true);
        assert!(resolve_and_dial("127.0.0.1", port, &opted_in).await.is_ok());
    }

    #[test]
    fn a_name_with_no_address_is_a_resolve_failure_and_all_refused_is_a_refusal() {
        let policy = DestinationPolicy::default();
        assert!(matches!(
            judge(Vec::new(), &policy),
            Err(DialError::Resolve)
        ));
        assert_eq!(DialError::Resolve.reason(), "resolve");
        let loopback = vec!["127.0.0.1:443".parse().unwrap()];
        assert!(matches!(
            judge(loopback, &policy),
            Err(DialError::Refused("loopback"))
        ));
    }

    #[test]
    fn the_whole_dial_ends_before_the_guest_gives_up() {
        assert!(DNS_TIMEOUT + DIAL_TIMEOUT < crate::protocol::OPEN_TIMEOUT);
        assert!(ATTEMPT_FLOOR < DIAL_TIMEOUT);
    }
}
