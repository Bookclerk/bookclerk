//! The window's system notifications: the inbox's alerts.
//!
//! The page reads the person's notification inbox; when an unread entry
//! arrives while the window is not focused and the person turned alerts on
//! for this computer, it calls the `notify` command with the entry's title
//! and text, and the shell posts a native notification (D-Bus on Linux, the
//! notification center on macOS, a toast on Windows). The text came from
//! the library's notification router, whose message rules already bound it;
//! the shell still trims it to what a system notification shows, so a
//! malformed call cannot post an unbounded or multi-line title.

/// Longest title posted, in characters.
pub(crate) const MAX_TITLE_CHARS: usize = 120;

/// Longest body posted, in characters.
pub(crate) const MAX_BODY_CHARS: usize = 500;

/// The application name the notification server shows.
const APP_NAME: &str = "Bookclerk";

/// One system notification, trimmed to what one shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Alert {
    /// One line, at most [`MAX_TITLE_CHARS`] characters, never empty.
    title: String,
    /// At most [`MAX_BODY_CHARS`] characters; line breaks kept.
    body: String,
}

impl Alert {
    /// Builds an alert from the page's title and text.
    ///
    /// Control characters become spaces (line breaks stay in the body),
    /// runs of spaces in the title fold into one, and each part is cut at a
    /// character boundary with an ellipsis when it is too long.
    ///
    /// # Arguments
    ///
    /// * `title` - The entry's title.
    /// * `body` - The entry's plain-text body.
    ///
    /// # Errors
    ///
    /// Returns an error when the title is empty once cleaned.
    pub(crate) fn new(title: &str, body: &str) -> Result<Self, String> {
        let title = clean(title, false)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if title.is_empty() {
            return Err("a notification needs a title".into());
        }
        Ok(Self {
            title: cut(&title, MAX_TITLE_CHARS),
            body: cut(clean(body, true).trim(), MAX_BODY_CHARS),
        })
    }

    /// Posts the alert. Blocks while the notification server answers, so
    /// callers run it off the main thread.
    ///
    /// # Errors
    ///
    /// Returns the system's refusal (no notification server, notifications
    /// turned off for the app).
    pub(crate) fn show(&self) -> Result<(), String> {
        let mut notification = notify_rust::Notification::new();
        notification
            .appname(APP_NAME)
            .summary(&self.title)
            .body(&body_for_server(&self.body));
        notification
            .show()
            .map(|_| ())
            .map_err(|err| format!("the system did not show the notification: {err}"))
    }
}

/// `body` as the notification server should receive it: freedesktop servers
/// that advertise `body-markup` read the body as simple markup, so plain
/// text is escaped for them (after trimming, so the limit counts the text
/// a person reads); other servers and platforms take it unchanged.
///
/// # Arguments
///
/// * `body` - The trimmed plain-text body.
///
/// # Returns
///
/// The body to send.
#[cfg(all(unix, not(target_os = "macos")))]
fn body_for_server(body: &str) -> String {
    let markup = notify_rust::get_capabilities()
        .is_ok_and(|caps| caps.iter().any(|cap| cap == "body-markup"));
    if markup {
        escape_markup(body)
    } else {
        body.to_owned()
    }
}

/// `body` as the notification server should receive it: unchanged on
/// platforms whose notifications do not read markup.
///
/// # Arguments
///
/// * `body` - The trimmed plain-text body.
///
/// # Returns
///
/// The body to send.
#[cfg(not(all(unix, not(target_os = "macos"))))]
fn body_for_server(body: &str) -> String {
    body.to_owned()
}

/// `text` with `&`, `<` and `>` written as entities, so a markup reader
/// shows them as typed.
///
/// # Arguments
///
/// * `text` - Plain text.
///
/// # Returns
///
/// The escaped text.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `text` with control characters replaced by spaces; line feeds stay when
/// `keep_lines` is set.
///
/// # Arguments
///
/// * `text` - The raw text.
/// * `keep_lines` - Whether `\n` survives.
///
/// # Returns
///
/// The cleaned text, the same length in characters.
fn clean(text: &str, keep_lines: bool) -> String {
    text.chars()
        .map(|c| {
            if c == '\n' && keep_lines {
                c
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// `text` cut to at most `max` characters, ending in `…` when cut.
///
/// # Arguments
///
/// * `text` - The text.
/// * `max` - The most characters kept, the ellipsis included (at least 1).
///
/// # Returns
///
/// The text, cut at a character boundary.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_one_line_and_never_empty() {
        let alert = Alert::new("Scan\n failed\t\u{7}now", "The scan\nstopped.\r").unwrap();
        assert_eq!(alert.title, "Scan failed now");
        assert_eq!(alert.body, "The scan\nstopped.");
        assert!(Alert::new(" \n\t", "body").is_err());
        assert_eq!(Alert::new("Title", "").unwrap().body, "");
    }

    #[test]
    fn markup_characters_are_written_as_entities() {
        assert_eq!(
            escape_markup("Open <b>Reports</b> & more"),
            "Open &lt;b&gt;Reports&lt;/b&gt; &amp; more"
        );
        assert_eq!(escape_markup("plain"), "plain");
    }

    #[test]
    fn long_text_is_cut_at_a_character_boundary() {
        let alert = Alert::new(&"é".repeat(400), &"ß ".repeat(600)).unwrap();
        assert_eq!(alert.title.chars().count(), MAX_TITLE_CHARS);
        assert!(alert.title.ends_with('…'));
        assert!(alert.body.chars().count() <= MAX_BODY_CHARS);
        assert!(alert.body.ends_with("ß…"));
        let exact = "a".repeat(MAX_TITLE_CHARS);
        assert_eq!(Alert::new(&exact, "").unwrap().title, exact);
    }
}
