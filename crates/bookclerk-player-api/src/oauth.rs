//! Sign-in for apps: player passwords and the browser sign-in.
//!
//! **Player passwords.** A patron makes one per app in Account settings;
//! the host generates it ([`format_player_password`]) and keeps only its
//! digest. It is 20 symbols from a 32-symbol alphabet with no look-alikes
//! (100 bits), shown in four groups of five. An app sends it as typed;
//! [`normalize_player_password`] drops spaces and dashes and ignores case
//! before the digest is looked up, and anything else is refused without a
//! lookup.
//!
//! **Browser sign-in.** Apps start with
//! `GET /player/auth/openid?code_challenge=..&code_challenge_method=S256&redirect_uri=<app>&client_id=..&response_type=code[&state=..]`
//! ([`parse_openid_start`]) and open the page the answer points to in a
//! browser: the library's own sign-in and consent page. Approving sends
//! the browser through `/player/auth/openid/mobile-redirect`
//! ([`parse_mobile_redirect`]) back to the app's address with a code, and
//! the app trades the code and its PKCE verifier at
//! `/player/auth/openid/callback` ([`parse_openid_callback`],
//! [`pkce_s256_matches`]) for a card. Only the apps in [`PLAYER_APPS`] can
//! be returned to.

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

/// The client id the host stores on a browser sign-in's code, so a code
/// minted for an app is redeemed only by the player API.
pub const PLAYER_CLIENT_ID: &str = "bookclerk-player";

/// How long a browser sign-in's code lives, in seconds.
pub const PLAYER_CODE_TTL_SECS: i64 = 300;

/// The path of the browser's way back to the app
/// ([`parse_mobile_redirect`]); the sign-in page sends every approval
/// there, whatever origin a request names.
pub const MOBILE_REDIRECT_PATH: &str = "/player/auth/openid/mobile-redirect";

/// The page a browser sign-in opens: the library's own sign-in and
/// consent page in the web app.
pub const CONSENT_PAGE_PATH: &str = "/apps/approve";

/// The alphabet of player passwords: digits and capital letters without
/// `0`, `1`, `I` and `O`.
pub const PASSWORD_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// Symbols in a player password.
pub const PASSWORD_SYMBOLS: usize = 20;

/// The longest query a sign-in route reads, in bytes.
pub const MAX_OAUTH_QUERY_BYTES: usize = 4 * 1024;

/// The longest `state` an app may send, in bytes.
pub const MAX_STATE_BYTES: usize = 512;

/// The apps a browser sign-in may return to: their redirect address and
/// the name the consent page shows.
pub const PLAYER_APPS: &[(&str, &str)] = &[
    ("audiobookshelf://oauth", "the Audiobookshelf app"),
    ("absorb://oauth", "Absorb"),
    ("lissen://oauth", "Lissen"),
];

/// A player password for 20 random bytes: each byte picks one symbol of
/// [`PASSWORD_ALPHABET`] (the alphabet divides 256, so every symbol is as
/// likely), in four dash-separated groups of five.
#[must_use]
pub fn format_player_password(random: &[u8; PASSWORD_SYMBOLS]) -> String {
    let mut out = String::with_capacity(PASSWORD_SYMBOLS + 3);
    for (i, byte) in random.iter().enumerate() {
        if i > 0 && i % 5 == 0 {
            out.push('-');
        }
        out.push(char::from(PASSWORD_ALPHABET[usize::from(byte & 31)]));
    }
    out
}

/// A typed player password in the one form its digest is taken of: the
/// symbols alone, in capitals.
///
/// # Returns
///
/// `None` unless, once spaces and dashes are dropped, it is exactly
/// [`PASSWORD_SYMBOLS`] symbols of [`PASSWORD_ALPHABET`] in either case.
#[must_use]
pub fn normalize_player_password(typed: &str) -> Option<String> {
    let mut out = String::with_capacity(PASSWORD_SYMBOLS);
    for c in typed.chars() {
        if c == '-' || c == ' ' {
            continue;
        }
        let upper = c.to_ascii_uppercase();
        if !upper.is_ascii() || !PASSWORD_ALPHABET.contains(&(upper as u8)) {
            return None;
        }
        if out.len() == PASSWORD_SYMBOLS {
            return None;
        }
        out.push(upper);
    }
    (out.len() == PASSWORD_SYMBOLS).then_some(out)
}

/// Why a sign-in request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthError {
    /// The query is longer than [`MAX_OAUTH_QUERY_BYTES`].
    TooLong,
    /// A parameter is missing or malformed; names it.
    Invalid(&'static str),
    /// The redirect address is not one of [`PLAYER_APPS`]. Apps look for
    /// the words `Invalid redirect_uri` in the answer.
    UnknownApp,
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong => f.write_str("query too long"),
            Self::Invalid(field) => write!(f, "invalid {field}"),
            Self::UnknownApp => f.write_str("Invalid redirect_uri"),
        }
    }
}

impl std::error::Error for OAuthError {}

/// The name of the app `redirect_uri` returns to, when it is one of
/// [`PLAYER_APPS`] exactly.
#[must_use]
pub fn app_for_redirect(redirect_uri: &str) -> Option<&'static str> {
    PLAYER_APPS
        .iter()
        .find(|(uri, _)| *uri == redirect_uri)
        .map(|(_, name)| *name)
}

/// The app a consent page's `redirect_uri` returns to: its `app`
/// parameter, when the address is a [`MOBILE_REDIRECT_PATH`] and the app
/// is one of [`PLAYER_APPS`]. Only the app is taken from the address; the
/// host always sends the browser to its own [`MOBILE_REDIRECT_PATH`].
///
/// # Returns
///
/// The app's redirect address and name.
#[must_use]
pub fn app_of_return_address(redirect_uri: &str) -> Option<(&'static str, &'static str)> {
    if redirect_uri.len() > MAX_OAUTH_QUERY_BYTES {
        return None;
    }
    let url = url::Url::parse(redirect_uri).ok()?;
    if url.path() != MOBILE_REDIRECT_PATH {
        return None;
    }
    let mut apps = url.query_pairs().filter(|(key, _)| key == "app");
    let app = apps.next()?.1;
    if apps.next().is_some() {
        return None;
    }
    PLAYER_APPS
        .iter()
        .find(|(uri, _)| *uri == app)
        .map(|(uri, name)| (*uri, *name))
}

/// True for a PKCE S256 challenge: 43 base64url symbols (a SHA-256 digest).
#[must_use]
pub fn is_s256_challenge(challenge: &str) -> bool {
    challenge.len() == 43
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// True for a PKCE verifier: 43 to 128 unreserved characters (RFC 7636).
#[must_use]
pub fn is_pkce_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// True when `verifier` is a PKCE verifier whose S256 digest is
/// `challenge`.
#[must_use]
pub fn pkce_s256_matches(verifier: &str, challenge: &str) -> bool {
    if !is_pkce_verifier(verifier) || !is_s256_challenge(challenge) {
        return false;
    }
    let digest = Sha256::digest(verifier.as_bytes());
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    // Both are 43 ASCII bytes; compare without an early exit.
    encoded
        .bytes()
        .zip(challenge.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Reads the pairs of a query, refusing one too long or with a key given
/// twice.
fn pairs(query: &str) -> Result<Vec<(String, String)>, OAuthError> {
    if query.len() > MAX_OAUTH_QUERY_BYTES {
        return Err(OAuthError::TooLong);
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if out.iter().any(|(seen, _)| *seen == key) {
            return Err(OAuthError::Invalid("query"));
        }
        out.push((key.into_owned(), value.into_owned()));
    }
    Ok(out)
}

/// The value of `key`, if present.
fn get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// A `state` value: printable ASCII, at most [`MAX_STATE_BYTES`].
fn state(pairs: &[(String, String)]) -> Result<Option<String>, OAuthError> {
    match get(pairs, "state") {
        None | Some("") => Ok(None),
        Some(s) if s.len() <= MAX_STATE_BYTES && s.bytes().all(|b| b.is_ascii_graphic()) => {
            Ok(Some(s.to_owned()))
        }
        Some(_) => Err(OAuthError::Invalid("state")),
    }
}

/// `GET /player/auth/openid`: an app starting a browser sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenIdStart {
    /// The app's own client name, echoed back (`client_id`).
    pub client_id: String,
    /// The app's address to return to: one of [`PLAYER_APPS`].
    pub redirect_uri: String,
    /// The app's name, from [`PLAYER_APPS`].
    pub app_name: &'static str,
    /// The app's `state`, when it sent one.
    pub state: Option<String>,
    /// The PKCE S256 challenge.
    pub code_challenge: String,
}

/// Parses an app's start of a browser sign-in.
///
/// # Errors
///
/// [`OAuthError::UnknownApp`] for a redirect address outside
/// [`PLAYER_APPS`]; [`OAuthError::Invalid`] for a missing or malformed
/// parameter (`response_type` must be `code` and the challenge method
/// `S256`); [`OAuthError::TooLong`] past [`MAX_OAUTH_QUERY_BYTES`].
pub fn parse_openid_start(query: &str) -> Result<OpenIdStart, OAuthError> {
    let pairs = pairs(query)?;
    if get(&pairs, "response_type").is_some_and(|t| t != "code") {
        return Err(OAuthError::Invalid("response_type"));
    }
    if get(&pairs, "code_challenge_method") != Some("S256") {
        return Err(OAuthError::Invalid("code_challenge_method"));
    }
    let code_challenge = get(&pairs, "code_challenge")
        .filter(|c| is_s256_challenge(c))
        .ok_or(OAuthError::Invalid("code_challenge"))?;
    let redirect_uri = get(&pairs, "redirect_uri").ok_or(OAuthError::Invalid("redirect_uri"))?;
    let app_name = app_for_redirect(redirect_uri).ok_or(OAuthError::UnknownApp)?;
    let client_id = get(&pairs, "client_id").unwrap_or("player-app");
    if client_id.is_empty()
        || client_id.len() > 64
        || !client_id.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
    {
        return Err(OAuthError::Invalid("client_id"));
    }
    Ok(OpenIdStart {
        client_id: client_id.to_owned(),
        redirect_uri: redirect_uri.to_owned(),
        app_name,
        state: state(&pairs)?,
        code_challenge: code_challenge.to_owned(),
    })
}

/// `GET /player/auth/openid/mobile-redirect`: the browser on its way back
/// to the app with a code (or a refusal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileRedirect {
    /// The app's address, one of [`PLAYER_APPS`] (`app`).
    pub app_redirect: String,
    /// The code, or `None` when the patron declined.
    pub code: Option<String>,
    /// The `state` to hand back.
    pub state: Option<String>,
}

/// Parses the browser's way back to the app.
///
/// # Errors
///
/// [`OAuthError::UnknownApp`] for an `app` outside [`PLAYER_APPS`];
/// [`OAuthError::Invalid`] for a malformed code or state.
pub fn parse_mobile_redirect(query: &str) -> Result<MobileRedirect, OAuthError> {
    let pairs = pairs(query)?;
    let app = get(&pairs, "app").ok_or(OAuthError::Invalid("app"))?;
    app_for_redirect(app).ok_or(OAuthError::UnknownApp)?;
    let code = match get(&pairs, "code") {
        None => None,
        Some(code) if is_s256_challenge(code) => Some(code.to_owned()),
        Some(_) => return Err(OAuthError::Invalid("code")),
    };
    Ok(MobileRedirect {
        app_redirect: app.to_owned(),
        code,
        state: state(&pairs)?,
    })
}

/// `GET /player/auth/openid/callback`: the app trading its code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenIdCallback {
    /// The code (43 base64url symbols, the shape the host mints).
    pub code: String,
    /// The PKCE verifier.
    pub code_verifier: String,
}

/// Parses an app's code exchange.
///
/// # Errors
///
/// [`OAuthError::Invalid`] for a missing or malformed code or verifier.
pub fn parse_openid_callback(query: &str) -> Result<OpenIdCallback, OAuthError> {
    let pairs = pairs(query)?;
    let code = get(&pairs, "code")
        .filter(|c| is_s256_challenge(c))
        .ok_or(OAuthError::Invalid("code"))?;
    let code_verifier = get(&pairs, "code_verifier")
        .filter(|v| is_pkce_verifier(v))
        .ok_or(OAuthError::Invalid("code_verifier"))?;
    Ok(OpenIdCallback {
        code: code.to_owned(),
        code_verifier: code_verifier.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_say_what_was_wrong() {
        assert_eq!(OAuthError::TooLong.to_string(), "query too long");
        assert_eq!(OAuthError::Invalid("state").to_string(), "invalid state");
        assert_eq!(OAuthError::UnknownApp.to_string(), "Invalid redirect_uri");
        assert_eq!(
            app_of_return_address(&"x".repeat(MAX_OAUTH_QUERY_BYTES + 1)),
            None
        );
        assert_eq!(app_of_return_address("not a url"), None);
    }

    #[test]
    fn passwords_are_generated_and_read_back() {
        let password = format_player_password(&[
            0, 1, 2, 3, 4, 31, 32, 63, 255, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
        ]);
        assert_eq!(password, "23456-Z2ZZB-CDEFG-HJKLM");
        assert_eq!(
            normalize_player_password(&password).as_deref(),
            Some("23456Z2ZZBCDEFGHJKLM")
        );
        assert_eq!(
            normalize_player_password(" 23456 z2zzb cdefg hjklm ").as_deref(),
            Some("23456Z2ZZBCDEFGHJKLM")
        );
        for bad in [
            "",
            "23456",
            "23456-ZZ2ZZ-BCDEF-GHJKLM",
            "O3456-ZZ2ZZ-BCDEF-GHJKL",
            "ü3456-ZZ2ZZ-BCDEF-GHJK",
        ] {
            assert_eq!(normalize_player_password(bad), None, "{bad}");
        }
    }

    #[test]
    fn pkce_checks_the_digest() {
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(pkce_s256_matches(verifier, challenge));
        assert!(!pkce_s256_matches(
            verifier,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cN"
        ));
        assert!(!pkce_s256_matches("short", challenge));
    }

    #[test]
    fn a_start_names_a_known_app() {
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let query = format!(
            "code_challenge={challenge}&code_challenge_method=S256&redirect_uri=audiobookshelf%3A%2F%2Foauth&client_id=Audiobookshelf-App&response_type=code"
        );
        let start = parse_openid_start(&query).unwrap();
        assert_eq!(start.app_name, "the Audiobookshelf app");
        assert_eq!(start.client_id, "Audiobookshelf-App");
        assert_eq!(start.state, None);
        let other = query.replace("audiobookshelf%3A%2F%2Foauth", "https%3A%2F%2Fevil.example");
        assert_eq!(parse_openid_start(&other), Err(OAuthError::UnknownApp));
        assert!(OAuthError::UnknownApp
            .to_string()
            .contains("Invalid redirect_uri"));
        let plain = query.replace("S256", "plain");
        assert!(parse_openid_start(&plain).is_err());
        let twice = format!("{query}&state=a&state=b");
        assert!(parse_openid_start(&twice).is_err());
    }

    #[test]
    fn a_return_address_names_only_a_known_app() {
        assert_eq!(
            app_of_return_address(
                "https://books.example/player/auth/openid/mobile-redirect?app=lissen%3A%2F%2Foauth"
            ),
            Some(("lissen://oauth", "Lissen"))
        );
        for bad in [
            "https://books.example/elsewhere?app=lissen%3A%2F%2Foauth",
            "https://books.example/player/auth/openid/mobile-redirect?app=evil%3A%2F%2Fx",
            "https://books.example/player/auth/openid/mobile-redirect?app=lissen%3A%2F%2Foauth&app=absorb%3A%2F%2Foauth",
            "not a url",
        ] {
            assert_eq!(app_of_return_address(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_way_back_and_the_exchange_are_bounded() {
        let code = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let back = parse_mobile_redirect(&format!("app=absorb%3A%2F%2Foauth&code={code}&state=s1"))
            .unwrap();
        assert_eq!(back.app_redirect, "absorb://oauth");
        assert_eq!(back.code.as_deref(), Some(code));
        assert_eq!(back.state.as_deref(), Some("s1"));
        assert_eq!(
            parse_mobile_redirect("app=x%3A%2F%2Fy"),
            Err(OAuthError::UnknownApp)
        );
        let exchange = parse_openid_callback(&format!(
            "state=s1&code={code}&code_verifier=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        ))
        .unwrap();
        assert_eq!(exchange.code, code);
        assert!(parse_openid_callback("code=x&code_verifier=y").is_err());
        assert_eq!(
            parse_openid_callback(&"a".repeat(MAX_OAUTH_QUERY_BYTES + 1)),
            Err(OAuthError::TooLong)
        );
    }
}
