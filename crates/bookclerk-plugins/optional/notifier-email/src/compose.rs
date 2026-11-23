//! Turns a rendered notification into an email, with no I/O of its own.
//!
//! The host renders the message (title, plain text, optional minimal HTML,
//! fields and a link built from the library's public origin). This module
//! lays those out as a plain-text body and a branded HTML body, compiled
//! from MJML with mrml so it renders alike in Outlook, Gmail and Apple Mail,
//! with the logo sent inside the message rather than fetched; then hands
//! them to [`build_message`].

use bookclerk_plugin_sdk::NotificationMessage;

use crate::smtp::{build_message, escape_html, message_id, InlineImage, MailContent};

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
/// link under its short label when the text does not already hold it, then
/// the footer after a signature separator.
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
            parts.push(format!("{}: {link}", link_label(link)));
        }
    }
    parts.retain(|part| !part.is_empty());
    format!("{}\n\n-- \n{FOOTER}\n", parts.join("\n\n"))
}

/// The Bookclerk mark the branded HTML shows beside the sender's name: a
/// 64-pixel PNG drawn at 32, so it stays sharp on dense screens.
pub const LOGO_PNG: &[u8] = include_bytes!("../assets/mark-64.png");

/// The `Content-ID` the HTML names the logo by (`cid:` URL).
pub const LOGO_CONTENT_ID: &str = "mark@bookclerk";

/// Brand colours, from the web app's palette (`ui/src/index.css`): paper
/// background, ink text, teal button with white text (4.9:1).
mod palette {
    /// Page background (`--color-paper`).
    pub const PAPER: &str = "#fbf7ee";
    /// Card background.
    pub const CARD: &str = "#ffffff";
    /// Body text (`--color-ink`).
    pub const INK: &str = "#0b3553";
    /// Secondary text on paper, 7:1.
    pub const MUTED: &str = "#4a5d6b";
    /// Button fill (`--color-teal-text`), white text at 6:1.
    pub const TEAL: &str = "#2f6b66";
    /// Hairlines (`--color-fold`).
    pub const LINE: &str = "#e7d3ac";
}

/// The footer under every message, in both parts (so the two carry the
/// same words, which spam filters compare).
pub const FOOTER: &str = "Sent by your library's Bookclerk server. This is an automatic message.";

/// A font stack every mail client has, so nothing is fetched to show it.
const FONT_STACK: &str =
    "-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif";

/// Most characters of the inbox preview line taken from the text.
const PREVIEW_CHARS: usize = 140;

/// The message's text as paragraphs: one per blank-line block, escaped, with
/// single line breaks kept.
///
/// # Arguments
///
/// * `text` - Plain text.
fn text_paragraphs(text: &str) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n");
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
        .map(|block| escape_html(block.trim()).replace('\n', "<br>"))
        .collect()
}

/// The host part of a link (`library.example.com`, with a port when one is
/// given), for the button's short label; `None` when the link has no
/// `scheme://host`.
///
/// # Arguments
///
/// * `link` - An absolute URL.
#[must_use]
pub fn link_host(link: &str) -> Option<&str> {
    let (_, rest) = link.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    (!host.is_empty()).then_some(host)
}

/// The label of the HTML's link button: short, and naming where it goes so
/// the recipient can trust it at a glance. The plain-text part keeps the
/// full URL.
///
/// # Arguments
///
/// * `link` - The message's link.
#[must_use]
pub fn link_label(link: &str) -> String {
    link_host(link).map_or_else(|| "Open the link".to_owned(), |host| format!("Open {host}"))
}

/// The inbox preview line: the text's first characters on one line.
///
/// # Arguments
///
/// * `text` - Plain text.
fn preview(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut: String = flat.chars().take(PREVIEW_CHARS).collect();
    if cut.len() < flat.len() {
        cut.push('…');
    }
    cut
}

/// The MJML source of the branded message: the sender's mark and name, the
/// title and body on a card, the fields as a table, the link as a button,
/// and a short footer.
///
/// # Arguments
///
/// * `message` - The rendered message.
/// * `sender` - The name shown beside the mark (the From name).
#[must_use]
pub fn mjml_source(message: &NotificationMessage, sender: &str) -> String {
    let title = escape_html(&message.title);
    let sender = escape_html(if sender.trim().is_empty() {
        "Bookclerk"
    } else {
        sender.trim()
    });
    let body = match message.html.as_deref() {
        Some(html) => format!("<mj-text>{html}</mj-text>"),
        None => text_paragraphs(&message.text)
            .iter()
            .map(|p| format!("<mj-text>{p}</mj-text>"))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let fields = if message.fields.is_empty() {
        String::new()
    } else {
        let rows: Vec<String> = message
            .fields
            .iter()
            .map(|field| {
                format!(
                    "<tr><th align=\"left\" style=\"padding:6px 12px 6px 0;border-top:1px solid {line};font-weight:600;vertical-align:top\">{}</th>\
                     <td style=\"padding:6px 0;border-top:1px solid {line};vertical-align:top\">{}</td></tr>",
                    escape_html(&field.name),
                    escape_html(&field.value),
                    line = palette::LINE,
                )
            })
            .collect();
        format!("<mj-table>{}</mj-table>", rows.join(""))
    };
    let button = match message.link.as_deref() {
        Some(link) if !body.contains(&escape_html(link)) => format!(
            "<mj-button href=\"{}\" align=\"left\" background-color=\"{}\" color=\"#ffffff\" \
             font-weight=\"600\" border-radius=\"6px\" inner-padding=\"12px 22px\">{}</mj-button>",
            escape_html(link),
            palette::TEAL,
            escape_html(&link_label(link)),
        ),
        _ => String::new(),
    };
    format!(
        r#"<mjml lang="en" dir="ltr">
<mj-head>
<mj-title>{title}</mj-title>
<mj-preview>{preview}</mj-preview>
<mj-attributes>
<mj-all font-family="{font}" />
<mj-text color="{ink}" font-size="15px" line-height="1.55" padding="6px 28px" />
<mj-table color="{ink}" font-size="14px" line-height="1.4" padding="10px 28px" />
<mj-button padding="16px 28px 8px" font-size="15px" />
</mj-attributes>
</mj-head>
<mj-body background-color="{paper}" width="560px">
<mj-section padding="24px 0 12px"><mj-column>
<mj-text padding="0 28px" font-size="14px" font-weight="600"><img src="cid:{logo}" width="28" height="28" alt="" style="vertical-align:middle;border:0;margin-right:8px">{sender}</mj-text>
</mj-column></mj-section>
<mj-section background-color="{card}" border-radius="10px" padding="18px 0 22px"><mj-column>
<mj-text font-size="20px" font-weight="600" line-height="1.3" padding="6px 28px 10px">{title}</mj-text>
{body}
{fields}
{button}
</mj-column></mj-section>
<mj-section padding="14px 0 28px"><mj-column>
<mj-text color="{muted}" font-size="12px" padding="0 28px">{footer}</mj-text>
</mj-column></mj-section>
</mj-body>
</mjml>"#,
        preview = escape_html(&preview(&message.text)),
        font = FONT_STACK,
        ink = palette::INK,
        paper = palette::PAPER,
        card = palette::CARD,
        muted = palette::MUTED,
        logo = LOGO_CONTENT_ID,
        footer = escape_html(FOOTER),
    )
}

/// Compiles MJML to HTML, with no web fonts (nothing is fetched to show the
/// message).
///
/// # Arguments
///
/// * `source` - MJML.
///
/// # Errors
///
/// The parser's or renderer's message when the source is not valid MJML.
pub fn render_mjml(source: &str) -> Result<String, String> {
    let parsed = mrml::parse(source).map_err(|err| err.to_string())?;
    let options = mrml::prelude::render::RenderOptions {
        fonts: std::collections::HashMap::new(),
        ..Default::default()
    };
    parsed
        .element
        .render(&options)
        .map_err(|err| err.to_string())
}

/// The plain HTML body, used when the branded template cannot be compiled
/// (for example host HTML that MJML's parser refuses): the content, the
/// fields as a table and the link under its short label, in a minimal
/// document with a language and a title.
///
/// # Arguments
///
/// * `message` - The rendered message.
#[must_use]
pub fn simple_html(message: &NotificationMessage) -> String {
    let content = match message.html.as_deref() {
        Some(html) => html.to_owned(),
        None => text_paragraphs(&message.text)
            .iter()
            .map(|p| format!("<p>{p}</p>"))
            .collect::<Vec<_>>()
            .join("\n"),
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
        Some(link) if !content.contains(&escape_html(link)) => format!(
            "\n<p><a href=\"{}\">{}</a></p>",
            escape_html(link),
            escape_html(&link_label(link))
        ),
        _ => String::new(),
    };
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>{}</title></head>\n<body>\n{content}{table}{link}\n</body></html>\n",
        escape_html(&message.title)
    )
}

/// The HTML body and whether it shows the logo: the branded template, or
/// [`simple_html`] when it cannot be compiled.
///
/// # Arguments
///
/// * `message` - The rendered message.
/// * `sender` - The name shown beside the mark.
#[must_use]
pub fn html_body(message: &NotificationMessage, sender: &str) -> (String, bool) {
    match render_mjml(&mjml_source(message, sender)) {
        Ok(html) => (html, true),
        Err(_) => (simple_html(message), false),
    }
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
    let (html, branded) = html_body(message, envelope.from_name);
    let logo = [InlineImage {
        content_id: LOGO_CONTENT_ID,
        content_type: "image/png",
        filename: "bookclerk.png",
        bytes: LOGO_PNG,
    }];
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
        images: if branded { &logo } else { &[] },
        message_id: &id,
        date: envelope.date,
        boundary: envelope.boundary,
    })
}
