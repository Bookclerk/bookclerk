//! The protocol half of the host's `/player` API: what audiobook player
//! apps written for an Audiobookshelf server send and expect, with no I/O
//! (`docs/adr/player-api.md`).
//!
//! The host (`bookclerkd`) mounts the routes and answers them from its
//! playback core and library store; this crate holds everything about the
//! wire that does not need the daemon, so the fuzz targets and the unit
//! tests drive the same code the server does:
//!
//! - [`request`]: sans-IO parsers for every body, header and query value
//!   an app sends (sign-in, refresh, play, sync, offline sessions,
//!   progress, bookmarks, list queries, filters), each bounded and typed.
//! - [`wire`]: the response shapes the apps read, with OpenAPI schemas.
//! - [`socket`]: the Engine.IO and Socket.IO framing of the
//!   websocket-only endpoint at `/player/socket.io`.
//! - [`oauth`]: player passwords and the browser sign-in's parameters.
//! - [`ids`]: the ids the API hands out for libraries, items and files.
//! - [`time`]: seconds on the wire, milliseconds in the playback core.
//! - [`tracks`]: how long each audio file of an item plays.
//!
//! Nothing here decides who may see or play what: the host applies the
//! patron's scope and the playback grant before it builds an answer.

pub mod ids;
pub mod oauth;
pub mod request;
pub mod socket;
pub mod time;
pub mod tracks;
pub mod wire;

/// The Audiobookshelf server release whose API this one answers like. Apps
/// turn features on by version (the refresh-token flow needs 2.26.0 or
/// later), so `/status` and the sign-in answers state it.
pub const COMPATIBLE_SERVER_VERSION: &str = "2.38.0";
