//! The response shapes player apps read.
//!
//! Field names and types follow what apps written for an Audiobookshelf
//! server parse: camelCase keys, lengths of time in seconds as JSON
//! numbers ([`crate::time`]), times of day in milliseconds since the Unix
//! epoch, and `null` (not an absent key) for an unknown value. Books are
//! the only media type: no shape here carries a podcast field, because
//! some apps tell a book from a podcast by which fields are present.
//!
//! Items carry one metadata shape whether listed or fetched alone: the
//! listing's flat names (`authorName`, `seriesName`) and the expanded
//! arrays (`authors`, `series`) together, so an app reading either finds
//! what it needs.

use serde::Serialize;
use utoipa::ToSchema;

/// The media type of every library and item the API lists.
pub const MEDIA_TYPE_BOOK: &str = "book";

/// The `app` the status answer names. Apps check it before they offer a
/// sign-in, so it is the value they expect.
pub const STATUS_APP: &str = "audiobookshelf";

/// The sign-in methods an app may offer: a player password, and the
/// browser sign-in through the library's own sign-in page.
pub const AUTH_METHODS: [&str; 2] = ["local", "openid"];

/// `GET /status`: what an app asks before it signs in.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// Always [`STATUS_APP`].
    #[schema(example = "audiobookshelf")]
    pub app: &'static str,
    /// The server release whose API this one answers like
    /// ([`crate::COMPATIBLE_SERVER_VERSION`]).
    #[schema(example = "2.17.0")]
    pub server_version: &'static str,
    /// Always true: the library is set up before an app can sign in.
    #[schema(example = true)]
    pub is_init: bool,
    /// The interface language apps show server text in.
    #[schema(example = "en-us")]
    pub language: &'static str,
    /// [`AUTH_METHODS`].
    #[schema(example = json!(["local", "openid"]))]
    pub auth_methods: Vec<&'static str>,
    /// What the sign-in screen shows.
    pub auth_form_data: AuthFormData,
}

/// Text an app shows on its sign-in screen.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuthFormData {
    /// A note under the sign-in form: where to get a player password.
    #[serde(rename = "authLoginCustomMessage")]
    #[schema(example = "Sign in with a player password from Settings, Account, Player apps.")]
    pub auth_login_custom_message: Option<String>,
    /// The label of the browser sign-in button.
    #[serde(rename = "authOpenIDButtonText")]
    #[schema(example = "Sign in with the library")]
    pub auth_openid_button_text: String,
    /// Whether apps open the browser sign-in without a tap: never.
    #[serde(rename = "authOpenIDAutoLaunch")]
    #[schema(example = false)]
    pub auth_openid_auto_launch: bool,
}

/// `GET /ping`: `{ "success": true }`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Ping {
    /// Always true.
    #[schema(example = true)]
    pub success: bool,
}

/// The answer to a sign-in, a token refresh, `POST /api/authorize` and
/// the browser sign-in's callback.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(as = PlayerLoginResponse)]
pub struct LoginResponse {
    /// The signed-in patron, with the card's tokens.
    pub user: User,
    /// The library an app opens first: the whole library.
    #[schema(example = "bookclerk-library")]
    pub user_default_library_id: String,
    /// Settings apps read at sign-in.
    pub server_settings: ServerSettings,
    /// E-reader devices: always empty, the API serves no ebooks.
    #[schema(value_type = Vec<Object>, example = json!([]))]
    pub ereader_devices: Vec<serde_json::Value>,
    /// How the server was installed; apps only display it.
    #[serde(rename = "Source")]
    #[schema(example = "bookclerk")]
    pub source: &'static str,
}

/// A patron as apps see them.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct User {
    /// The patron's portal identity id, as a string.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// The name the patron signs in with.
    #[schema(example = "riley")]
    pub username: String,
    /// The patron's email address, if the library has one.
    #[schema(example = "riley@example.com")]
    pub email: Option<String>,
    /// Always `user`: no app is given administrator powers.
    #[serde(rename = "type")]
    #[schema(example = "user")]
    pub kind: &'static str,
    /// The access token, for apps that read it from here (older apps
    /// send this one as their bearer). Empty where the answer is not a
    /// sign-in.
    #[schema(example = "eyJhbGciOi...")]
    pub token: String,
    /// The card's access token, on sign-in and refresh answers only.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "eyJhbGciOi...")]
    pub access_token: Option<String>,
    /// The card's refresh token, on sign-in and refresh answers only.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "eyJhbGciOi...")]
    pub refresh_token: Option<String>,
    /// Always false: tokens are never the old non-expiring kind.
    #[schema(example = false)]
    pub is_old_token: bool,
    /// Progress on every item the patron has started.
    pub media_progress: Vec<MediaProgress>,
    /// Series hidden from continue listening: none.
    #[schema(example = json!([]))]
    pub series_hide_from_continue_listening: Vec<String>,
    /// Every bookmark the patron has made.
    pub bookmarks: Vec<Bookmark>,
    /// Always true: a disabled patron cannot sign in.
    #[schema(example = true)]
    pub is_active: bool,
    /// Always false.
    #[schema(example = false)]
    pub is_locked: bool,
    /// When the patron was last seen, in epoch milliseconds.
    #[schema(example = 1763630000000_i64)]
    pub last_seen: Option<i64>,
    /// When the patron joined, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub created_at: i64,
    /// What the patron may do.
    pub permissions: Permissions,
    /// Libraries the patron may open, by id.
    #[schema(example = json!(["bookclerk-library"]))]
    pub libraries_accessible: Vec<String>,
    /// Tags that limit what the patron sees: none (scope is applied by
    /// the host, not by tags).
    #[schema(example = json!([]))]
    pub item_tags_selected: Vec<String>,
    /// Whether the patron signed in through the browser at least once.
    #[serde(rename = "hasOpenIDLink")]
    #[schema(example = false)]
    pub has_openid_link: bool,
}

/// What a patron may do, as apps read it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Permissions {
    /// Whether the patron may download items to the device.
    #[schema(example = true)]
    pub download: bool,
    /// Always false: apps edit nothing.
    #[schema(example = false)]
    pub update: bool,
    /// Always false.
    #[schema(example = false)]
    pub delete: bool,
    /// Always false.
    #[schema(example = false)]
    pub upload: bool,
    /// Always false.
    #[schema(example = false)]
    pub create_ereader: bool,
    /// Always true: the libraries listed are the ones the patron sees.
    #[schema(example = true)]
    pub access_all_libraries: bool,
    /// Always true.
    #[schema(example = true)]
    pub access_all_tags: bool,
    /// Always true: scope, not an explicit flag, decides visibility.
    #[schema(example = true)]
    pub access_explicit_content: bool,
    /// Always false.
    #[schema(example = false)]
    pub selected_tags_not_accessible: bool,
}

/// The server settings apps read.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettings {
    /// Always `server-settings`.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: &'static str,
    /// [`crate::COMPATIBLE_SERVER_VERSION`]: apps turn features on by it.
    #[schema(example = "2.17.0")]
    pub version: &'static str,
    /// Always 1.
    #[schema(example = 1)]
    pub build_number: u32,
    /// The interface language.
    #[schema(example = "en-us")]
    pub language: &'static str,
    /// How apps format dates.
    #[schema(example = "MM/dd/yyyy")]
    pub date_format: &'static str,
    /// How apps format times.
    #[schema(example = "HH:mm")]
    pub time_format: &'static str,
    /// Whether sorting skips leading articles: no.
    #[schema(example = true)]
    pub sorting_ignore_prefix: bool,
    /// The articles sorting would skip.
    #[schema(example = json!(["the", "a"]))]
    pub sorting_prefixes: Vec<&'static str>,
    /// Whether apps offer casting: no.
    #[schema(example = false)]
    pub chromecast_enabled: bool,
    /// The home shelf style (1: standard).
    #[schema(example = 1)]
    pub home_bookshelf_view: u8,
    /// The library shelf style (1: standard).
    #[schema(example = 1)]
    pub bookshelf_view: u8,
    /// [`AUTH_METHODS`].
    #[schema(example = json!(["local", "openid"]))]
    pub auth_active_auth_methods: Vec<&'static str>,
    /// The browser sign-in button's label.
    #[serde(rename = "authOpenIDButtonText")]
    #[schema(example = "Sign in with the library")]
    pub auth_openid_button_text: String,
    /// Never true.
    #[serde(rename = "authOpenIDAutoLaunch")]
    #[schema(example = false)]
    pub auth_openid_auto_launch: bool,
    /// The sign-in screen's note.
    #[schema(example = "Sign in with a player password from Settings, Account, Player apps.")]
    pub auth_login_custom_message: Option<String>,
    /// The host's time zone name, or UTC.
    #[schema(example = "UTC")]
    pub time_zone: String,
}

/// `GET /api/libraries`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Libraries {
    /// The libraries the patron may open.
    pub libraries: Vec<Library>,
}

/// One library an app can open.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    /// The library id ([`crate::ids::WHOLE_LIBRARY_ID`] for the whole
    /// library).
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// The library's name.
    #[schema(example = "Ann Author")]
    pub name: String,
    /// Folders on disk: none are shown.
    #[schema(value_type = Vec<Object>)]
    #[schema(example = json!([]))]
    pub folders: Vec<serde_json::Value>,
    /// The library's place in the list, from 1.
    #[schema(example = 1)]
    pub display_order: u32,
    /// The icon apps show (`audiobookshelf` names a book stack).
    #[schema(example = "audiobookshelf")]
    pub icon: &'static str,
    /// Always [`MEDIA_TYPE_BOOK`].
    #[schema(example = "book")]
    pub media_type: &'static str,
    /// The metadata provider apps show: none.
    #[schema(example = "audible")]
    pub provider: &'static str,
    /// Display settings.
    pub settings: LibrarySettings,
    /// Always null.
    #[schema(example = json!(null))]
    pub last_scan: Option<i64>,
    /// Always null.
    #[schema(example = "2.17.0")]
    pub last_scan_version: Option<String>,
    /// When the library was made, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub created_at: i64,
    /// When it last changed, in epoch milliseconds.
    #[schema(example = 1763630000000_i64)]
    pub last_update: i64,
}

/// A library's display settings.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibrarySettings {
    /// 1 for square covers, 2 for the 1.6:1 book shape: square.
    #[schema(example = 1)]
    pub cover_aspect_ratio: u8,
    /// Always true: the library is not a watched folder.
    #[schema(example = false)]
    pub disable_watcher: bool,
    /// Always true: audiobooks only.
    #[schema(example = true)]
    pub audiobooks_only: bool,
    /// Whether one-book series are hidden from the series list: no.
    #[schema(example = false)]
    pub hide_single_book_series: bool,
    /// Whether continue series skips earlier books: no.
    #[schema(example = false)]
    pub only_show_later_books_in_continue_series: bool,
    /// Always false.
    #[schema(example = false)]
    pub epubs_allow_scripted_content: bool,
    /// An item counts as finished at this share listened, in percent.
    #[schema(example = json!(null))]
    pub mark_as_finished_percent_complete: Option<f64>,
    /// An item counts as finished with this many seconds left.
    #[schema(example = 10)]
    pub mark_as_finished_time_remaining: f64,
}

/// `GET /api/libraries/{id}?include=filterdata`.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibraryWithFilterData {
    /// The filter values.
    pub filterdata: FilterData,
    /// Items with problems: none are reported.
    #[schema(example = 0)]
    pub issues: u64,
    /// The patron's playlists in the library: none.
    #[schema(example = 0)]
    pub num_user_playlists: u64,
    /// The library.
    pub library: Library,
}

/// The values an app offers as filters.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilterData {
    /// Authors with at least one item.
    pub authors: Vec<NamedRef>,
    /// Genres.
    #[schema(example = json!(["Fantasy"]))]
    pub genres: Vec<String>,
    /// Tags: none are shown.
    #[schema(example = json!([]))]
    pub tags: Vec<String>,
    /// Series with at least one item.
    pub series: Vec<NamedRef>,
    /// Narrators.
    #[schema(example = json!(["Nia Narrator"]))]
    pub narrators: Vec<String>,
    /// Languages.
    #[schema(example = json!(["en-us"]))]
    pub languages: Vec<String>,
    /// Publishers.
    #[schema(example = json!(["Example Press"]))]
    pub publishers: Vec<String>,
    /// Decades of publication: none are offered.
    #[schema(example = json!(["2020"]))]
    pub published_decades: Vec<String>,
    /// Items in the library.
    #[schema(example = 1)]
    pub book_count: u64,
    /// Authors in the library.
    #[schema(example = 1)]
    pub author_count: u64,
    /// Series in the library.
    #[schema(example = 1)]
    pub series_count: u64,
    /// Always 0.
    #[schema(example = 0)]
    pub podcast_count: u64,
    /// Always 0.
    #[schema(example = 0)]
    pub number_episodes: u64,
    /// When the values were read, in epoch milliseconds.
    #[schema(example = 1763630000000_i64)]
    pub loaded_at: i64,
}

/// An id and a name.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NamedRef {
    /// The entity's id.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// Its display name.
    #[schema(example = "Ann Author")]
    pub name: String,
}

/// A series an item belongs to, with the item's place in it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SeriesRef {
    /// The series id.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// The series name.
    #[schema(example = "Ann Author")]
    pub name: String,
    /// The item's place in the series as the library writes it (`1`,
    /// `2.5`), if known.
    #[schema(example = "1")]
    pub sequence: Option<String>,
}

/// A page of items: `GET /api/libraries/{id}/items` and the like.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemPage {
    /// The page's items.
    pub results: Vec<LibraryItem>,
    /// Items matching the query in all.
    #[schema(example = 1)]
    pub total: u64,
    /// Items per page (0: no paging).
    #[schema(example = 20)]
    pub limit: u64,
    /// The page number, from 0.
    #[schema(example = 0)]
    pub page: u64,
    /// The sort key the app sent, echoed.
    #[schema(example = "media.metadata.title")]
    pub sort_by: Option<String>,
    /// Whether the order is reversed.
    #[schema(example = false)]
    pub sort_desc: bool,
    /// The filter the app sent, echoed.
    #[schema(example = json!(null))]
    pub filter_by: Option<String>,
    /// Always [`MEDIA_TYPE_BOOK`].
    #[schema(example = "book")]
    pub media_type: &'static str,
    /// Whether items are minified.
    #[schema(example = false)]
    pub minified: bool,
    /// Whether series were collapsed: never.
    #[schema(example = false)]
    pub collapseseries: bool,
    /// The include list the app sent, echoed.
    #[schema(example = "")]
    pub include: String,
    /// The offset of the first result.
    #[schema(example = 0)]
    pub offset: u64,
}

/// An item: an edition with a stored copy.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibraryItem {
    /// The item id: the edition id.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// The item's inode as apps expect one: the edition id again.
    #[schema(example = "0")]
    pub ino: String,
    /// Always null.
    #[schema(example = json!(null))]
    pub old_library_item_id: Option<String>,
    /// The library the item is listed in.
    #[schema(example = "bookclerk-library")]
    pub library_id: String,
    /// The folder id: the library id.
    #[schema(example = "bookclerk-library")]
    pub folder_id: String,
    /// A display path: the title.
    #[schema(example = "/the-first.m4b")]
    pub path: String,
    /// A display path: the title.
    #[schema(example = "the-first.m4b")]
    pub rel_path: String,
    /// Always false: an item is a folder of tracks.
    #[schema(example = true)]
    pub is_file: bool,
    /// When the copy last changed, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub mtime_ms: i64,
    /// When the copy was stored, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub ctime_ms: i64,
    /// When the copy was stored, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub birthtime_ms: i64,
    /// When the item joined the library, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub added_at: i64,
    /// When its record last changed, in epoch milliseconds.
    #[schema(example = 1763630000000_i64)]
    pub updated_at: i64,
    /// Always false.
    #[schema(example = false)]
    pub is_missing: bool,
    /// Always false.
    #[schema(example = false)]
    pub is_invalid: bool,
    /// Always [`MEDIA_TYPE_BOOK`].
    #[schema(example = "book")]
    pub media_type: &'static str,
    /// The book.
    pub media: BookMedia,
    /// Files in the item.
    #[schema(example = 1)]
    pub num_files: u64,
    /// Bytes in all its files.
    #[schema(example = 100000)]
    pub size: u64,
    /// The item's files, when the item is fetched expanded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_files: Option<Vec<LibraryFile>>,
    /// The patron's progress, when the app asked for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_media_progress: Option<MediaProgress>,
    /// The item's place in a series, on a series' book list.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "1")]
    pub series_sequence: Option<String>,
}

/// A book's media: metadata, tracks and chapters.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BookMedia {
    /// The media id: the edition id.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// The item id, when fetched expanded.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "li_8f14e45fceea467f")]
    pub library_item_id: Option<String>,
    /// The book's metadata.
    pub metadata: BookMetadata,
    /// A cover path apps only test for presence; covers are fetched from
    /// `/api/items/{id}/cover`.
    #[schema(example = "/player/api/items/li_8f14e45fceea467f/cover")]
    pub cover_path: Option<String>,
    /// Tags: none.
    #[schema(example = json!([]))]
    pub tags: Vec<String>,
    /// Tracks in play order.
    #[schema(example = 1)]
    pub num_tracks: u64,
    /// Audio files.
    #[schema(example = 1)]
    pub num_audio_files: u64,
    /// Chapters.
    #[schema(example = 0)]
    pub num_chapters: u64,
    /// The book's length in seconds.
    #[schema(example = 36000.0)]
    pub duration: f64,
    /// Bytes in its audio files.
    #[schema(example = 100000)]
    pub size: u64,
    /// The audio files, when fetched expanded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_files: Option<Vec<AudioFile>>,
    /// The chapters, when fetched expanded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chapters: Option<Vec<Chapter>>,
    /// The tracks with their place in the book, when fetched expanded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracks: Option<Vec<AudioTrack>>,
}

/// A book's metadata.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    /// The title.
    #[schema(example = "The First")]
    pub title: String,
    /// The title for sorting.
    #[schema(example = "First, The")]
    pub title_ignore_prefix: String,
    /// The subtitle, if any.
    #[schema(example = "A Novel")]
    pub subtitle: Option<String>,
    /// The authors.
    pub authors: Vec<NamedRef>,
    /// The authors' names joined with `, `.
    #[schema(example = "Ann Author")]
    pub author_name: String,
    /// The authors' names, family name first, joined with `, `.
    #[serde(rename = "authorNameLF")]
    #[schema(example = "Author, Ann")]
    pub author_name_lf: String,
    /// The narrators' names.
    #[schema(example = json!(["Nia Narrator"]))]
    pub narrators: Vec<String>,
    /// The narrators' names joined with `, `.
    #[schema(example = "Nia Narrator")]
    pub narrator_name: String,
    /// The series the book is in.
    pub series: Vec<SeriesRef>,
    /// The series names with the book's place (`Name #2`), joined.
    #[schema(example = "The Saga #1")]
    pub series_name: String,
    /// Genres.
    #[schema(example = json!(["Fantasy"]))]
    pub genres: Vec<String>,
    /// The year of publication, as a string (`"2019"`).
    #[schema(example = "2024")]
    pub published_year: Option<String>,
    /// The date of publication, if known.
    #[schema(example = "2024-03-01")]
    pub published_date: Option<String>,
    /// The publisher.
    #[schema(example = "Example Press")]
    pub publisher: Option<String>,
    /// The description, as the library stores it.
    #[schema(example = "A short description.")]
    pub description: Option<String>,
    /// The description with markup removed.
    #[schema(example = "A short description.")]
    pub description_plain: Option<String>,
    /// The ISBN, if known.
    #[schema(example = "9780000000002")]
    pub isbn: Option<String>,
    /// The ASIN, if known.
    #[schema(example = "B000000000")]
    pub asin: Option<String>,
    /// The language.
    #[schema(example = "en-us")]
    pub language: Option<String>,
    /// Always false.
    #[schema(example = false)]
    pub explicit: bool,
    /// Whether the edition is abridged.
    #[schema(example = false)]
    pub abridged: bool,
}

/// A file of an item, in the expanded item's `libraryFiles`.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibraryFile {
    /// The file's `ino` ([`crate::ids::file_ino`]).
    #[schema(example = "0")]
    pub ino: String,
    /// The file's name, size and times.
    pub metadata: FileMetadata,
    /// When the file was stored, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub added_at: i64,
    /// When the file was stored, in epoch milliseconds.
    #[schema(example = 1763630000000_i64)]
    pub updated_at: i64,
    /// Always `audio`.
    #[schema(example = "audio")]
    pub file_type: &'static str,
}

/// A file's name, size and times.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileMetadata {
    /// The file name.
    #[schema(example = "the-first.m4b")]
    pub filename: String,
    /// The extension with its dot (`.m4b`).
    #[schema(example = ".m4b")]
    pub ext: String,
    /// A display path: the file name.
    #[schema(example = "/the-first.m4b")]
    pub path: String,
    /// A display path: the file name.
    #[schema(example = "the-first.m4b")]
    pub rel_path: String,
    /// The file's size in bytes.
    #[schema(example = 100000)]
    pub size: u64,
    /// When the file was stored, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub mtime_ms: i64,
    /// When the file was stored, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub ctime_ms: i64,
    /// When the file was stored, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub birthtime_ms: i64,
}

/// An audio file of a book.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AudioFile {
    /// The file's place in play order, from 1.
    #[schema(example = 1)]
    pub index: u64,
    /// The file's `ino` ([`crate::ids::file_ino`]).
    #[schema(example = "0")]
    pub ino: String,
    /// The file's name, size and times.
    pub metadata: FileMetadata,
    /// When the file was stored, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub added_at: i64,
    /// When the file was stored, in epoch milliseconds.
    #[schema(example = 1763630000000_i64)]
    pub updated_at: i64,
    /// The track number the file's tags state: not read.
    #[schema(example = 1)]
    pub track_num_from_meta: Option<u64>,
    /// The disc number the file's tags state: not read.
    #[schema(example = json!(null))]
    pub disc_num_from_meta: Option<u64>,
    /// The track number in the file name: not read.
    #[schema(example = json!(null))]
    pub track_num_from_filename: Option<u64>,
    /// The disc number in the file name: not read.
    #[schema(example = json!(null))]
    pub disc_num_from_filename: Option<u64>,
    /// Always false.
    #[schema(example = false)]
    pub manually_verified: bool,
    /// Always false.
    #[schema(example = false)]
    pub exclude: bool,
    /// Always null.
    #[schema(example = json!(null))]
    pub error: Option<String>,
    /// The container format, such as `mp4` or `mp3`.
    #[schema(example = "MP4A")]
    pub format: Option<String>,
    /// The file's length in seconds.
    #[schema(example = 36000.0)]
    pub duration: f64,
    /// Bits per second, if known.
    #[schema(example = 128000)]
    pub bit_rate: Option<u64>,
    /// The file's language, if known.
    #[schema(example = "en-us")]
    pub language: Option<String>,
    /// The codec, if known.
    #[schema(example = "aac")]
    pub codec: Option<String>,
    /// The time base, if known.
    #[schema(example = "1/44100")]
    pub time_base: Option<String>,
    /// Audio channels, if known.
    #[schema(example = 2)]
    pub channels: Option<u64>,
    /// The channel layout, if known.
    #[schema(example = "stereo")]
    pub channel_layout: Option<String>,
    /// Chapters embedded in the file: none are read.
    pub chapters: Vec<Chapter>,
    /// Always null.
    #[schema(example = json!(null))]
    pub embedded_cover_art: Option<String>,
    /// Tags read from the file: none until the stored media probe reads
    /// them.
    pub meta_tags: MetaTags,
    /// The file's media type, such as `audio/mp4`.
    #[schema(example = "audio/mp4")]
    pub mime_type: String,
}

/// A track with its place in the book.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AudioTrack {
    /// The track's place in play order, from 1.
    #[schema(example = 1)]
    pub index: u64,
    /// Where the track starts in the book, in seconds.
    #[schema(example = 0.0)]
    pub start_offset: f64,
    /// The track's length in seconds.
    #[schema(example = 36000.0)]
    pub duration: f64,
    /// The track's title: its file name.
    #[schema(example = "The First")]
    pub title: String,
    /// The path the track streams from, under the API's base address
    /// (`/api/items/{id}/file/{ino}`); apps add `?token=`.
    #[schema(example = "/player/api/items/li_8f14e45fceea467f/file/0")]
    pub content_url: String,
    /// The track's media type.
    #[schema(example = "audio/mp4")]
    pub mime_type: String,
    /// The codec, if known.
    #[schema(example = "aac")]
    pub codec: Option<String>,
    /// The file's name, size and times.
    pub metadata: Option<FileMetadata>,
}

/// A chapter of a book.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Chapter {
    /// The chapter's place, from 0.
    #[schema(example = 1)]
    pub id: u64,
    /// Where it starts, in seconds.
    #[schema(example = 0.0)]
    pub start: f64,
    /// Where it ends, in seconds.
    #[schema(example = 600.0)]
    pub end: f64,
    /// Its title.
    #[schema(example = "The First")]
    pub title: String,
}

/// The patron's progress on one item.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MediaProgress {
    /// The progress id: the item id (one record per patron and item).
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// The patron's id.
    #[schema(example = "42")]
    pub user_id: String,
    /// The item id.
    #[schema(example = "li_8f14e45fceea467f")]
    pub library_item_id: String,
    /// Always null.
    #[schema(example = json!(null))]
    pub episode_id: Option<String>,
    /// The media id: the item id.
    #[schema(example = "li_8f14e45fceea467f")]
    pub media_item_id: String,
    /// Always `book`.
    #[schema(example = "book")]
    pub media_item_type: &'static str,
    /// The book's length in seconds, as last reported.
    #[schema(example = 36000.0)]
    pub duration: f64,
    /// The share listened, from 0 to 1.
    #[schema(example = 0.05)]
    pub progress: f64,
    /// Where the patron is, in seconds.
    #[schema(example = 1830.5)]
    pub current_time: f64,
    /// Whether the patron finished it.
    #[schema(example = false)]
    pub is_finished: bool,
    /// Whether it is hidden from continue listening.
    #[schema(example = false)]
    pub hide_from_continue_listening: bool,
    /// Always null.
    #[schema(example = json!(null))]
    pub ebook_location: Option<String>,
    /// Always null.
    #[schema(example = json!(null))]
    pub ebook_progress: Option<f64>,
    /// When the progress last changed, in epoch milliseconds.
    #[schema(example = 1763630000000_i64)]
    pub last_update: i64,
    /// When the patron started, in epoch milliseconds.
    #[schema(example = 1763620000000_i64)]
    pub started_at: i64,
    /// When the patron finished, in epoch milliseconds.
    #[schema(example = json!(null))]
    pub finished_at: Option<i64>,
}

/// A bookmark.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Bookmark {
    /// The item it is in.
    #[schema(example = "li_8f14e45fceea467f")]
    pub library_item_id: String,
    /// Its title.
    #[schema(example = "The First")]
    pub title: String,
    /// Where it is, in seconds.
    #[schema(example = 1830.0)]
    pub time: f64,
    /// When it was made, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub created_at: i64,
}

/// The device a playback session was opened from.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionDevice {
    /// The device's id: the card's session id.
    #[schema(example = "pixel-8")]
    pub device_id: String,
    /// The device name the app reported, if any.
    #[schema(example = "Pixel 8")]
    pub device_name: Option<String>,
    /// The app's name, if reported.
    #[schema(example = "Absorb")]
    pub client_name: Option<String>,
}

/// A playback session, as `POST /api/items/{id}/play` answers.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackSession {
    /// The session id; apps send it back to sync and close.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// The patron's id.
    #[schema(example = "42")]
    pub user_id: String,
    /// The library id.
    #[schema(example = "bookclerk-library")]
    pub library_id: String,
    /// The item id.
    #[schema(example = "li_8f14e45fceea467f")]
    pub library_item_id: String,
    /// The media id: the item id.
    #[schema(example = "li_8f14e45fceea467f")]
    pub book_id: String,
    /// Always null.
    #[schema(example = json!(null))]
    pub episode_id: Option<String>,
    /// Always [`MEDIA_TYPE_BOOK`].
    #[schema(example = "book")]
    pub media_type: &'static str,
    /// The book's metadata.
    pub media_metadata: BookMetadata,
    /// The book's chapters.
    pub chapters: Vec<Chapter>,
    /// The title apps show.
    #[schema(example = "The First")]
    pub display_title: String,
    /// The author apps show.
    #[schema(example = "Ann Author")]
    pub display_author: String,
    /// A cover path apps test for presence.
    #[schema(example = "/player/api/items/li_8f14e45fceea467f/cover")]
    pub cover_path: Option<String>,
    /// The book's length in seconds.
    #[schema(example = 36000.0)]
    pub duration: f64,
    /// How the app plays: 0, direct play of the stored files.
    #[schema(example = 1)]
    pub play_method: u8,
    /// The player the app reported.
    #[schema(example = "Absorb")]
    pub media_player: String,
    /// The device the session was opened from.
    pub device_info: SessionDevice,
    /// [`crate::COMPATIBLE_SERVER_VERSION`].
    #[schema(example = "2.17.0")]
    pub server_version: &'static str,
    /// The day the session started, `YYYY-MM-DD` in UTC.
    #[schema(example = "2026-11-20")]
    pub date: String,
    /// The weekday the session started, in English.
    #[schema(example = "Friday")]
    pub day_of_week: String,
    /// Seconds listened in this session.
    #[schema(example = 60.0)]
    pub time_listening: f64,
    /// Where the session started, in seconds.
    #[schema(example = 1770.5)]
    pub start_time: f64,
    /// Where the patron is, in seconds.
    #[schema(example = 1830.5)]
    pub current_time: f64,
    /// When the session started, in epoch milliseconds.
    #[schema(example = 1763620000000_i64)]
    pub started_at: i64,
    /// When it last synced, in epoch milliseconds.
    #[schema(example = 1763630000000_i64)]
    pub updated_at: i64,
    /// The tracks to play, in order.
    pub audio_tracks: Vec<AudioTrack>,
    /// The item, expanded.
    pub library_item: Option<LibraryItem>,
}

/// `GET /api/me/listening-sessions`.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionPage {
    /// Sessions in all.
    #[schema(example = 1)]
    pub total: u64,
    /// Pages in all.
    #[schema(example = 1)]
    pub num_pages: u64,
    /// The page number, from 0.
    #[schema(example = 0)]
    pub page: u64,
    /// Sessions per page.
    #[schema(example = 20)]
    pub items_per_page: u64,
    /// The page's sessions, newest first.
    pub sessions: Vec<PlaybackSession>,
}

/// `GET /api/me/listening-stats`.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListeningStats {
    /// Seconds listened in all.
    #[schema(example = 7200.0)]
    pub total_time: f64,
    /// Seconds listened per item, by item id.
    #[schema(example = json!({}))]
    pub items: std::collections::BTreeMap<String, ItemListening>,
    /// Seconds listened per day, by `YYYY-MM-DD`.
    #[schema(example = json!({"2026-11-20": 1800.0}))]
    pub days: std::collections::BTreeMap<String, f64>,
    /// Seconds listened per weekday, by English weekday name.
    #[schema(example = json!({"Friday": 1800.0}))]
    pub day_of_week: std::collections::BTreeMap<String, f64>,
    /// Seconds listened today.
    #[schema(example = 1800.0)]
    pub today: f64,
    /// The most recent sessions.
    pub recent_sessions: Vec<PlaybackSession>,
}

/// Seconds listened to one item.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemListening {
    /// The item id.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// Seconds listened.
    #[schema(example = 60.0)]
    pub time_listening: f64,
    /// The item's metadata.
    pub media_metadata: BookMetadata,
}

/// `{ "libraryItems": [...] }`: in-progress items and batch reads.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibraryItems {
    /// The items.
    pub library_items: Vec<LibraryItem>,
}

/// A series.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Series {
    /// The series id.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// Its name.
    #[schema(example = "Ann Author")]
    pub name: String,
    /// The name for sorting.
    #[schema(example = "Saga, The")]
    pub name_ignore_prefix: String,
    /// Its description: none.
    #[schema(example = "A short description.")]
    pub description: Option<String>,
    /// When the library first saw it, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub added_at: i64,
    /// When it last changed, in epoch milliseconds.
    #[schema(example = 1763630000000_i64)]
    pub updated_at: i64,
    /// The library it is listed in.
    #[schema(example = "bookclerk-library")]
    pub library_id: String,
    /// Its items in series order, on lists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub books: Option<Vec<LibraryItem>>,
    /// The patron's progress through it, when asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<SeriesProgress>,
}

/// The patron's progress through a series.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeriesProgress {
    /// The series' items, by id.
    #[schema(example = json!(["li_8f14e45fceea467f"]))]
    pub library_item_ids: Vec<String>,
    /// Those the patron finished.
    #[schema(example = json!([]))]
    pub library_item_ids_finished: Vec<String>,
    /// Whether the patron finished every one.
    #[schema(example = false)]
    pub is_finished: bool,
}

/// A page of series.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeriesPage {
    /// The page's series.
    pub results: Vec<Series>,
    /// Series in all.
    #[schema(example = 1)]
    pub total: u64,
    /// Series per page.
    #[schema(example = 20)]
    pub limit: u64,
    /// The page number, from 0.
    #[schema(example = 0)]
    pub page: u64,
    /// The sort key, echoed.
    #[schema(example = "media.metadata.title")]
    pub sort_by: Option<String>,
    /// Whether the order is reversed.
    #[schema(example = false)]
    pub sort_desc: bool,
    /// The filter, echoed.
    #[schema(example = json!(null))]
    pub filter_by: Option<String>,
    /// Whether items are minified.
    #[schema(example = false)]
    pub minified: bool,
    /// The include list, echoed.
    #[schema(example = "")]
    pub include: String,
}

/// An author.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Author {
    /// The contributor id.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// Always null.
    #[schema(example = "B000000000")]
    pub asin: Option<String>,
    /// The author's name.
    #[schema(example = "Ann Author")]
    pub name: String,
    /// The name family name first.
    #[schema(example = "Author, Ann")]
    pub last_first: String,
    /// Always null.
    #[schema(example = "A short description.")]
    pub description: Option<String>,
    /// Always null: no author images.
    #[schema(example = json!(null))]
    pub image_path: Option<String>,
    /// The library it is listed in.
    #[schema(example = "bookclerk-library")]
    pub library_id: String,
    /// When the library first saw the author, in epoch milliseconds.
    #[schema(example = 1763600000000_i64)]
    pub added_at: i64,
    /// When the record last changed, in epoch milliseconds.
    #[schema(example = 1763630000000_i64)]
    pub updated_at: i64,
    /// Items by the author the patron can see.
    #[schema(example = 1)]
    pub num_books: u64,
    /// The author's items, when asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_items: Option<Vec<LibraryItem>>,
    /// The author's series with their items, when asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series: Option<Vec<AuthorSeries>>,
}

/// A series of an author's, with the author's items in it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AuthorSeries {
    /// The series id.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// The series name.
    #[schema(example = "Ann Author")]
    pub name: String,
    /// The items, in series order.
    pub items: Vec<LibraryItem>,
}

/// `GET /api/libraries/{id}/authors` without paging.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Authors {
    /// The authors.
    pub authors: Vec<Author>,
}

/// A page of authors.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthorPage {
    /// The page's authors.
    pub results: Vec<Author>,
    /// Authors in all.
    pub total: u64,
    /// Authors per page.
    pub limit: u64,
    /// The page number, from 0.
    pub page: u64,
}

/// A narrator with how many items they read.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Narrator {
    /// The name in URL-safe base64, as apps filter by it.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// The narrator's name.
    #[schema(example = "Ann Author")]
    pub name: String,
    /// Items they read that the patron can see.
    #[schema(example = 1)]
    pub num_books: u64,
}

/// `GET /api/libraries/{id}/narrators`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Narrators {
    /// The narrators.
    pub narrators: Vec<Narrator>,
}

/// A personalized shelf on the home screen.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Shelf {
    /// The shelf id, such as `continue-listening`.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: &'static str,
    /// The shelf's English label.
    #[schema(example = "Continue Listening")]
    pub label: &'static str,
    /// The key apps translate the label by.
    #[schema(example = "LabelContinueListening")]
    pub label_string_key: &'static str,
    /// What the shelf holds: `book`, `series` or `authors`.
    #[serde(rename = "type")]
    #[schema(example = "user")]
    pub kind: &'static str,
    /// The shelf's entries.
    pub entities: ShelfEntities,
    /// Entries in all.
    #[schema(example = 1)]
    pub total: u64,
}

/// A shelf's entries.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(untagged)]
pub enum ShelfEntities {
    /// Items.
    Books(Vec<LibraryItem>),
    /// Series.
    Series(Vec<Series>),
    /// Authors.
    Authors(Vec<Author>),
}

/// `GET /api/libraries/{id}/search`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SearchResults {
    /// Matching items.
    pub book: Vec<SearchBook>,
    /// Always empty.
    #[schema(value_type = Vec<Object>)]
    #[schema(example = json!([]))]
    pub podcast: Vec<serde_json::Value>,
    /// Matching narrators.
    pub narrators: Vec<Narrator>,
    /// Always empty.
    #[schema(value_type = Vec<Object>)]
    #[schema(example = json!([]))]
    pub tags: Vec<serde_json::Value>,
    /// Always empty.
    #[schema(value_type = Vec<Object>)]
    #[schema(example = json!([]))]
    pub genres: Vec<serde_json::Value>,
    /// Matching series.
    pub series: Vec<SearchSeries>,
    /// Matching authors.
    pub authors: Vec<Author>,
}

/// An item that matched a search.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchBook {
    /// The item.
    pub library_item: LibraryItem,
}

/// A series that matched a search.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SearchSeries {
    /// The series.
    pub series: Series,
    /// Its items.
    pub books: Vec<LibraryItem>,
}

/// `GET /api/libraries/{id}/stats`.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStats {
    /// Items.
    #[schema(example = 1)]
    pub total_items: u64,
    /// Authors.
    #[schema(example = 1)]
    pub total_authors: u64,
    /// Genres.
    #[schema(example = 1)]
    pub total_genres: u64,
    /// Seconds of audio in all.
    #[schema(example = 36000.0)]
    pub total_duration: f64,
    /// Audio tracks in all.
    #[schema(example = 1)]
    pub num_audio_tracks: u64,
    /// Bytes in all: not counted.
    #[schema(example = 100000)]
    pub total_size: u64,
}

/// An empty page, for collections and playlists the library does not
/// have yet.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EmptyPage {
    /// Always empty.
    #[schema(value_type = Vec<Object>)]
    #[schema(example = json!([]))]
    pub results: Vec<serde_json::Value>,
    /// Always 0.
    #[schema(example = 1)]
    pub total: u64,
    /// Items per page, echoed.
    #[schema(example = 20)]
    pub limit: u64,
    /// The page number, echoed.
    #[schema(example = 0)]
    pub page: u64,
}

/// `{ "playlists": [] }`: the patron's playlists.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Playlists {
    /// Always empty.
    #[schema(value_type = Vec<Object>)]
    pub playlists: Vec<serde_json::Value>,
}

/// One entry's outcome in a batch sync.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SyncResult {
    /// The session or item id the entry named.
    #[schema(example = "li_8f14e45fceea467f")]
    pub id: String,
    /// Whether it was applied.
    #[schema(example = true)]
    pub success: bool,
    /// Why not, when it was not.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = json!(null))]
    pub error: Option<String>,
}

/// `POST /api/session/local-all`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SyncResults {
    /// One outcome per entry, in the order sent.
    pub results: Vec<SyncResult>,
}

/// `user_item_progress_updated` on the socket: progress changed on another
/// device.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    /// The progress id.
    pub id: String,
    /// The session that changed it, if one did.
    pub session_id: Option<String>,
    /// The device that changed it.
    pub device_description: String,
    /// The progress.
    pub data: MediaProgress,
}

/// `init` on the socket: the socket is signed in.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SocketInit {
    /// The patron's id.
    pub user_id: String,
    /// The patron's sign-in name.
    pub username: String,
}

/// `auth_failed` on the socket.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SocketAuthFailed {
    /// Why, in English.
    pub message: &'static str,
}

/// The name an item's title sorts by: a leading `The `, `A ` or `An `
/// moved to the end (`Hobbit, The`), as apps show it in sorted lists.
#[must_use]
pub fn title_ignore_prefix(title: &str) -> String {
    for prefix in ["The ", "A ", "An "] {
        if let Some(rest) = title.strip_prefix(prefix) {
            if !rest.is_empty() {
                return format!("{rest}, {}", prefix.trim_end());
            }
        }
    }
    title.to_owned()
}

/// A name family name first (`Le Guin, Ursula K.`): the last word moved to
/// the front. A one-word name is returned as it is.
#[must_use]
pub fn last_first(name: &str) -> String {
    let name = name.trim();
    match name.rsplit_once(' ') {
        Some((given, family)) if !given.trim().is_empty() => format!("{family}, {}", given.trim()),
        _ => name.to_owned(),
    }
}

/// Text with markup removed: everything between `<` and `>` dropped and
/// the common entities decoded.
#[must_use]
pub fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_names_move_articles_and_family_names() {
        assert_eq!(title_ignore_prefix("The Hobbit"), "Hobbit, The");
        assert_eq!(title_ignore_prefix("An Echo"), "Echo, An");
        assert_eq!(title_ignore_prefix("The "), "The ");
        assert_eq!(title_ignore_prefix("Dune"), "Dune");
        assert_eq!(last_first("Ursula K. Le Guin"), "Guin, Ursula K. Le");
        assert_eq!(last_first("Plato"), "Plato");
        assert_eq!(last_first("  Frank Herbert "), "Herbert, Frank");
    }

    #[test]
    fn markup_is_removed() {
        assert_eq!(plain_text("<p>One &amp; <b>two</b></p>"), "One & two");
        assert_eq!(plain_text("a < b"), "a ");
        assert_eq!(plain_text(""), "");
    }

    #[test]
    fn progress_has_the_wire_names() {
        let progress = MediaProgress {
            id: "e".into(),
            user_id: "1".into(),
            library_item_id: "e".into(),
            episode_id: None,
            media_item_id: "e".into(),
            media_item_type: "book",
            duration: 10.0,
            progress: 0.5,
            current_time: 5.0,
            is_finished: false,
            hide_from_continue_listening: false,
            ebook_location: None,
            ebook_progress: None,
            last_update: 1,
            started_at: 1,
            finished_at: None,
        };
        let json = serde_json::to_value(&progress).unwrap();
        assert_eq!(json["libraryItemId"], "e");
        assert_eq!(json["currentTime"], 5.0);
        assert!(json["episodeId"].is_null());
        assert!(json.get("episodes").is_none());
    }
}

/// The tags read from an audio file, as apps read them: always empty until
/// the stored media probe reads tags, so it serializes as `{}`.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct MetaTags {}
