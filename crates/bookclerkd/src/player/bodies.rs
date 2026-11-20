//! The request bodies of the player API, as the OpenAPI document shows
//! them.
//!
//! The routes read their bodies with the bounded, sans-IO parsers in
//! `bookclerk_player_api::request` (fuzzed as `player_request`), not with
//! serde, so these types only describe what the parsers accept. They are
//! never constructed.

/// `POST /player/login`: a sign-in with a player password.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)] // Described, never constructed: `parse_login_request` reads the body.
pub(crate) struct LoginBody {
    /// The patron's sign-in name or email address.
    #[schema(example = "riley")]
    username: String,
    /// A player password from Account settings; the account password is
    /// never accepted.
    #[schema(example = "ABCDE-FGHJK-LMNPQ-RSTUV")]
    password: String,
}

/// The device an app plays on, as it describes itself.
#[derive(utoipa::ToSchema)]
#[schema(rename_all = "camelCase")]
#[allow(dead_code)] // Described, never constructed: `parse_play_request` reads the body.
pub(crate) struct DeviceInfoBody {
    /// The app's name.
    #[schema(example = "Absorb")]
    client_name: Option<String>,
    /// The device's maker.
    #[schema(example = "Google")]
    manufacturer: Option<String>,
    /// The device's model.
    #[schema(example = "Pixel 9")]
    model: Option<String>,
    /// The name the patron gave the device.
    #[schema(example = "Riley's phone")]
    device_name: Option<String>,
}

/// `POST /player/api/items/{id}/play`: what an app asks for when it starts
/// playing. Tracks always play directly, whatever it asks.
#[derive(utoipa::ToSchema)]
#[schema(rename_all = "camelCase")]
#[allow(dead_code)] // Described, never constructed: `parse_play_request` reads the body.
pub(crate) struct PlayBody {
    /// The device, named in the patron's session list.
    device_info: Option<DeviceInfoBody>,
    /// Whether the app asks for direct play; tracks always play directly.
    #[schema(example = true)]
    force_direct_play: Option<bool>,
    /// Whether the app asks for transcoding; it gets direct tracks too.
    #[schema(example = false)]
    force_transcode: Option<bool>,
    /// The app's player, shown in the session list.
    #[schema(example = "exoplayer")]
    media_player: Option<String>,
}

/// `POST /player/api/session/{id}/sync` and `.../close`: where the patron
/// is.
#[derive(utoipa::ToSchema)]
#[schema(rename_all = "camelCase")]
#[allow(dead_code)] // Described, never constructed: `parse_sync_request` reads the body.
pub(crate) struct SyncBody {
    /// The position, in seconds.
    #[schema(example = 754.2)]
    current_time: Option<f64>,
    /// Seconds listened since the last sync.
    #[schema(example = 15.0)]
    time_listened: Option<f64>,
    /// The runtime the app measured, in seconds.
    #[schema(example = 36000.0)]
    duration: Option<f64>,
}

/// `POST /player/api/session/local`: a stretch of listening the app made
/// offline.
#[derive(utoipa::ToSchema)]
#[schema(rename_all = "camelCase")]
#[allow(dead_code)] // Described, never constructed: `parse_local_session` reads the body.
pub(crate) struct LocalSessionBody {
    /// The app's own id for the session.
    #[schema(example = "6f9c1d2e-offline")]
    id: String,
    /// The item.
    #[schema(example = "li_8f14e45fceea467f")]
    library_item_id: String,
    /// The position, in seconds.
    #[schema(example = 754.2)]
    current_time: f64,
    /// Seconds listened in the session.
    #[schema(example = 600.0)]
    time_listening: Option<f64>,
    /// The runtime the app measured, in seconds.
    #[schema(example = 36000.0)]
    duration: Option<f64>,
    /// When the app last updated the session, in epoch milliseconds.
    #[schema(example = 1763630000000_i64)]
    updated_at: Option<i64>,
}

/// `POST /player/api/session/local-all`: several offline sessions.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)] // Described, never constructed: `parse_local_sessions` reads the body.
pub(crate) struct LocalSessionsBody {
    /// The sessions; each is answered on its own.
    sessions: Vec<LocalSessionBody>,
}

/// `PATCH /player/api/me/progress/{id}`: a change to the patron's progress.
#[derive(utoipa::ToSchema)]
#[schema(rename_all = "camelCase")]
#[allow(dead_code)] // Described, never constructed: `parse_progress_patch` reads the body.
pub(crate) struct ProgressBody {
    /// The position, in seconds.
    #[schema(example = 754.2)]
    current_time: Option<f64>,
    /// The share of `duration` listened, 0 to 1, read when `currentTime`
    /// is absent.
    #[schema(example = 0.02)]
    progress: Option<f64>,
    /// The runtime, in seconds.
    #[schema(example = 36000.0)]
    duration: Option<f64>,
    /// Whether the book is finished.
    #[schema(example = false)]
    is_finished: Option<bool>,
    /// True to take the item off Continue listening.
    #[schema(example = false)]
    hide_from_continue_listening: Option<bool>,
}

/// One entry of `PATCH /player/api/me/progress/batch/update`.
#[derive(utoipa::ToSchema)]
#[schema(rename_all = "camelCase")]
#[allow(dead_code)] // Described, never constructed: `parse_progress_batch` reads the body.
pub(crate) struct ProgressBatchEntry {
    /// The item.
    #[schema(example = "li_8f14e45fceea467f")]
    library_item_id: String,
    /// The position, in seconds.
    #[schema(example = 754.2)]
    current_time: Option<f64>,
    /// The runtime, in seconds.
    #[schema(example = 36000.0)]
    duration: Option<f64>,
    /// Whether the book is finished.
    #[schema(example = true)]
    is_finished: Option<bool>,
}

/// `POST` and `PATCH /player/api/me/item/{id}/bookmark`.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)] // Described, never constructed: `parse_bookmark_request` reads the body.
pub(crate) struct BookmarkBody {
    /// The place, in seconds; on `PATCH`, which bookmark to rename.
    #[schema(example = 1830.0)]
    time: f64,
    /// The bookmark's title.
    #[schema(example = "The storm begins")]
    title: Option<String>,
}

/// `POST /player/api/items/batch/get`.
#[derive(utoipa::ToSchema)]
#[schema(rename_all = "camelCase")]
#[allow(dead_code)] // Described, never constructed: `parse_batch_get` reads the body.
pub(crate) struct BatchGetBody {
    /// The items, at most 100.
    #[schema(example = json!(["li_8f14e45fceea467f"]))]
    library_item_ids: Vec<String>,
}
