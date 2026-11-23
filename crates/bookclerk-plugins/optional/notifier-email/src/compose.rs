//! Turns a rendered notification into an email, with no I/O of its own.
//!
//! The host renders the message (title, plain text, optional minimal HTML,
//! fields and a link built from the library's public origin). This module
//! lays those out as a plain-text body and an HTML body, then hands them to
//! [`build_message`].

use bookclerk_plugin_sdk::NotificationMessage;

use crate::smtp::{build_message, escape_html, message_id, MailContent};

/// Who sends one email, and to whom.
#[derive(Debug, Clone)]
pub struct Envelope<'a> {
    /// Sender address, checked.
    pub from: &'a str,
    /// Sender display name; may be empty.
    pub from_name: &'a str,
    /// Recipient addresses, checked.
    pub to: &'a [String],
    /// Host id of the destination, part of the stable `Message-ID`.
    pub destination_id: &'a str,
    /// When it is sent, as an RFC 5322 date.
    pub date: &'a str,
    /// MIME boundary.
    pub boundary: &'a str,
}

/// The plain-text body: the text, then each field on its own line, then the
/// link when the text does not already hold it.
///
/// # Arguments
///
/// * `message` - The rendered message.
#[must_use]
pub fn plain_body(message: &NotificationMessage) -> String {
    let mut parts = vec![message.text.trim_end().to_owned()];
    if !message.fields.is_empty() {
        parts.push(
            message
                .fields
                .iter()
                .map(|field| format!("{}: {}", field.name, field.value))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if let Some(link) = message.link.as_deref() {
        if !message.text.contains(link) {
            parts.push(link.to_owned());
        }
    }
    parts.retain(|part| !part.is_empty());
    format!("{}\n", parts.join("\n\n"))
}

/// The HTML body: the host's HTML (or the text, escaped, one paragraph per
/// blank-line block), the fields as a table and the link as an anchor, in a
/// minimal document.
///
/// # Arguments
///
/// * `message` - The rendered message.
#[must_use]
pub fn html_body(message: &NotificationMessage) -> String {
    let content = match message.html.as_deref() {
        Some(html) => html.to_owned(),
        None => {
            let normalized = message.text.replace("\r\n", "\n");
            let mut blocks = Vec::new();
            let mut current: Vec<&str> = Vec::new();
            for line in normalized.split('\n') {
                if line.trim().is_empty() {
                    if !current.is_empty() {
                        blocks.push(std::mem::take(&mut current).join("\n"));
                    }
                } else {
                    current.push(line);
                }
            }
            if !current.is_empty() {
                blocks.push(current.join("\n"));
            }
            blocks
                .iter()
                .map(|block| format!("<p>{}</p>", escape_html(block.trim()).replace('\n', "<br>")))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    let rows: Vec<String> = message
        .fields
        .iter()
        .map(|field| {
            format!(
                "<tr><th align=\"left\">{}</th><td>{}</td></tr>",
                escape_html(&field.name),
                escape_html(&field.value)
            )
        })
        .collect();
    let table = if rows.is_empty() {
        String::new()
    } else {
        format!("\n<table cellpadding=\"4\">\n{}\n</table>", rows.join("\n"))
    };
    let link = match message.link.as_deref() {
        Some(link) if !content.contains(&escape_html(link)) => {
            let escaped = escape_html(link);
            format!("\n<p><a href=\"{escaped}\">{escaped}</a></p>")
        }
        _ => String::new(),
    };
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"></head>\n<body>\n{content}{table}{link}\n</body></html>\n"
    )
}

/// Builds the whole email for one destination.
///
/// # Arguments
///
/// * `message` - The rendered message.
/// * `envelope` - Sender, recipients and the per-send values.
///
/// # Returns
///
/// The message, ready for DATA (before dot-stuffing).
#[must_use]
pub fn compose(message: &NotificationMessage, envelope: &Envelope<'_>) -> String {
    let text = plain_body(message);
    let html = html_body(message);
    let id = message_id(
        &message.notification_id,
        envelope.destination_id,
        envelope.from,
    );
    build_message(&MailContent {
        from: envelope.from,
        from_name: envelope.from_name,
        to: envelope.to,
        subject: &message.title,
        text: &text,
        html: Some(&html),
        message_id: &id,
        date: envelope.date,
        boundary: envelope.boundary,
    })
}
