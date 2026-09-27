//! Identity of a live socket-proxy endpoint.
//!
//! The tag is the answer from the server that owns the pipe. A numeric fd or
//! handle in some other process is not that identity.

/// `true` when `reported` is a real answer and it is not `foreign`.
///
/// An empty report is not absence: the probe did not exercise an endpoint.
#[must_use]
pub fn foreign_channel_absent(reported: &str, foreign: &str) -> bool {
    !reported.is_empty() && !foreign.is_empty() && reported != foreign
}
