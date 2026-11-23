//! Email notifier for Bookclerk: one SMTP submission per message.
//!
//! The guest is a native binary, because TLS for a raw TCP protocol is not
//! available to workerd plugins: it reaches the operator's submission server
//! through a host tunnel (checked against the approved `server` setting) and
//! runs TLS itself with rustls, either from the first byte or after
//! `STARTTLS`. See `docs/server/notifications.md#the-email-notifier`.
//!
//! - [`smtp`]: replies, capabilities, addresses, the message and the
//!   session state machine, with no I/O.
//! - [`compose`]: a rendered notification as text and HTML bodies.
//! - [`client`]: moves a session's bytes over a connection.
//! - [`notifier`]: the settings and the `notifier` entrypoint.

pub mod client;
pub mod compose;
pub mod notifier;
pub mod smtp;
