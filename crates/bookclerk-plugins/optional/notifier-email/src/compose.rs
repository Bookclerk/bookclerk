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
/// * `sender` - The From name, the library's name in the link's label.
#[must_use]
pub fn plain_body(message: &NotificationMessage, sender: &str) -> String {
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
            parts.push(format!("{}: {link}", link_label(sender)));
        }
    }
    if introduces_bookclerk(message) {
        parts.push(format!(
            "{INTRO_TITLE}\n{INTRO_TEXT}\nLearn more at bookclerk.app: {BOOKCLERK_SITE}"
        ));
    }
    parts.retain(|part| !part.is_empty());
    format!("{}\n\n-- \n{}\n", parts.join("\n\n"), plain_footer(message))
}

/// The Bookclerk mark the branded HTML shows in the header: a
/// 64-pixel PNG drawn at 32, so it stays sharp on dense screens.
pub const LOGO_PNG: &[u8] = include_bytes!("../assets/mark-64.png");

/// The `Content-ID` the HTML names the logo by (`cid:` URL).
pub const LOGO_CONTENT_ID: &str = "mark@bookclerk";

/// Brand colours, from the website's and web app's palette
/// (`tools/site/src/site.css`, `ui/src/index.css`): a library index card on
/// paper, ink text, a brick rule across the card's top, and the site's ink
/// primary button.
mod palette {
    /// Page background (`--paper`).
    pub const PAPER: &str = "#fbf7ee";
    /// Card background (`--index-card`).
    pub const CARD: &str = "#fffdf7";
    /// Introduction panel (`--parchment`).
    pub const PARCHMENT: &str = "#f3e5c6";
    /// Body text and the button's fill (`--ink`); paper on ink is 13:1.
    pub const INK: &str = "#0b3553";
    /// Secondary text on paper, 7:1.
    pub const MUTED: &str = "#4a5d6b";
    /// The card's top rule (`--brick`).
    pub const BRICK: &str = "#c84a34";
    /// Hairlines (`--fold`).
    pub const LINE: &str = "#e7d3ac";
}

/// The serif the website sets headings in (Literata), then serifs every
/// mail client has; nothing is fetched.
const SERIF_STACK: &str = "Literata, Georgia, 'Times New Roman', serif";

/// The notification type the host gives invite mail: its recipient may not
/// know Bookclerk yet, so it carries the introduction.
pub const INVITE_TYPE: &str = "account_invite";

/// Whether a message carries the introduction: only invites do.
///
/// # Arguments
///
/// * `message` - The rendered message.
fn introduces_bookclerk(message: &NotificationMessage) -> bool {
    message.notification_type == INVITE_TYPE
}

/// The introduction's heading, for recipients who may not know Bookclerk.
pub const INTRO_TITLE: &str = "New to Bookclerk?";

/// The introduction, in both parts.
pub const INTRO_TEXT: &str = "Bookclerk is a home for audiobooks. It gathers the books a household has bought from multiple audiobook storefronts into one library that runs on its owner's own computer. With your sign-in you can browse the shelves, listen in your browser and keep your own wishlist. It's free and open source.";

/// Bookclerk's own site, linked from every footer.
pub const BOOKCLERK_SITE: &str = "https://bookclerk.app";

/// The footer's closing sentence, in both parts.
const FOOTER_NOTE: &str = "This is an automatic message.";

/// The footer under every message as plain text: who sent it (the library's
/// home page when the host gave one) and Bookclerk's site. The HTML footer
/// says the same words, which spam filters compare.
///
/// # Arguments
///
/// * `message` - The rendered message.
#[must_use]
pub fn plain_footer(message: &NotificationMessage) -> String {
    match library_home(message) {
        Some(home) => format!(
            "Sent by your library at {home} with Bookclerk ({BOOKCLERK_SITE}). {FOOTER_NOTE}"
        ),
        None => format!("Sent by your library with Bookclerk ({BOOKCLERK_SITE}). {FOOTER_NOTE}"),
    }
}

/// The library's home page, when the host gave one.
///
/// # Arguments
///
/// * `message` - The rendered message.
fn library_home(message: &NotificationMessage) -> Option<&str> {
    message
        .library_origin
        .as_deref()
        .map(str::trim)
        .filter(|origin| link_host(origin).is_some())
}

/// The website's body face (Source Sans 3), then sans-serifs every mail
/// client has; nothing is fetched.
const FONT_STACK: &str =
    "'Source Sans 3', -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif";

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
/// given), for the footer's link to the library's home page; `None` when the
/// link has no `scheme://host`.
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

/// Whether a From name names a library rather than just the product (empty
/// or `Bookclerk`, which the header already shows).
///
/// # Arguments
///
/// * `sender` - The From name.
fn names_library(sender: &str) -> bool {
    let sender = sender.trim();
    !sender.is_empty() && !sender.eq_ignore_ascii_case("Bookclerk")
}

/// The label of the link button: short, naming the library it opens
/// (`Open Riverside Library`), or `Open your library` when the From name
/// is just the product's. The plain-text part keeps the full URL beside it.
///
/// # Arguments
///
/// * `sender` - The From name, which names the library.
#[must_use]
pub fn link_label(sender: &str) -> String {
    if names_library(sender) {
        format!("Open {}", sender.trim())
    } else {
        "Open your library".to_owned()
    }
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

/// The MJML source of the branded message: the Bookclerk mark and name as
/// the header; a card with the sender (the From name), the title, the body,
/// the fields as a table and the link as a button; and a footer linking the
/// library's home page and Bookclerk's site.
///
/// # Arguments
///
/// * `message` - The rendered message.
/// * `sender` - The From name, shown at the top of the card.
#[must_use]
pub fn mjml_source(message: &NotificationMessage, sender: &str) -> String {
    let title = escape_html(&message.title);
    // The card names who it is from (the From name), unless that is just
    // the product name the header already shows.
    let eyebrow = if !names_library(sender) {
        String::new()
    } else {
        format!(
            "<mj-text color=\"{}\" font-size=\"13px\" font-weight=\"600\" padding=\"4px 28px 0\">{}</mj-text>\n",
            palette::MUTED,
            escape_html(sender.trim())
        )
    };
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
            "<mj-button href=\"{}\" align=\"left\" background-color=\"{}\" color=\"{}\" \
             font-weight=\"600\" border-radius=\"6px\" inner-padding=\"13px 22px\">{}</mj-button>",
            escape_html(link),
            palette::INK,
            palette::PAPER,
            escape_html(&link_label(sender)),
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
<mj-text padding="0 28px" font-family="{serif}" font-size="19px" font-weight="700"><img src="cid:{logo}" width="30" height="30" alt="" style="vertical-align:middle;border:0;margin-right:10px">Bookclerk</mj-text>
</mj-column></mj-section>
<mj-section background-color="{card}" border-top="4px solid {brick}" border-radius="6px" padding="20px 0 24px"><mj-column>
{eyebrow}<mj-text font-family="{serif}" font-size="23px" font-weight="700" line-height="1.25" padding="6px 28px 12px">{title}</mj-text>
{body}
{fields}
{button}
</mj-column></mj-section>
{intro}
<mj-section padding="16px 0 28px"><mj-column>
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
        footer = html_footer(message),
        serif = SERIF_STACK,
        brick = palette::BRICK,
        intro = intro_section(message),
    )
}

/// The introduction panel for invites, on parchment under the card; empty
/// otherwise.
///
/// # Arguments
///
/// * `message` - The rendered message.
fn intro_section(message: &NotificationMessage) -> String {
    if !introduces_bookclerk(message) {
        return String::new();
    }
    format!(
        "<mj-section padding=\"0\"><mj-column><mj-spacer height=\"14px\" /></mj-column></mj-section>\n\
         <mj-section background-color=\"{}\" border-radius=\"6px\" padding=\"14px 0 16px\"><mj-column>\n\
         <mj-text font-family=\"{SERIF_STACK}\" font-size=\"17px\" font-weight=\"700\" padding=\"4px 28px 2px\">{}</mj-text>\n\
         <mj-text font-size=\"14px\" padding=\"4px 28px\">{}</mj-text>\n\
         <mj-text font-size=\"14px\" font-weight=\"600\" padding=\"4px 28px\"><a href=\"{BOOKCLERK_SITE}\" style=\"color:{};text-decoration:underline\">Learn more at bookclerk.app</a></mj-text>\n\
         </mj-column></mj-section>",
        palette::PARCHMENT,
        escape_html(INTRO_TITLE),
        escape_html(INTRO_TEXT),
        palette::INK,
    )
}

/// The HTML footer: the same words as [`plain_footer`], with the library's
/// host and Bookclerk as links.
///
/// # Arguments
///
/// * `message` - The rendered message.
fn html_footer(message: &NotificationMessage) -> String {
    let link = |href: &str, text: &str| {
        format!(
            "<a href=\"{}\" style=\"color:{};text-decoration:underline\">{}</a>",
            escape_html(href),
            palette::MUTED,
            escape_html(text)
        )
    };
    let bookclerk = link(BOOKCLERK_SITE, "Bookclerk");
    match library_home(message) {
        Some(home) => format!(
            "Sent by your library at {} with {bookclerk}. {FOOTER_NOTE}",
            link(home, link_host(home).unwrap_or(home))
        ),
        None => format!("Sent by your library with {bookclerk}. {FOOTER_NOTE}"),
    }
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
/// * `sender` - The From name, the library's name in the link's label.
#[must_use]
pub fn simple_html(message: &NotificationMessage, sender: &str) -> String {
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
            escape_html(&link_label(sender))
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
/// * `sender` - The From name, shown at the top of the card.
#[must_use]
pub fn html_body(message: &NotificationMessage, sender: &str) -> (String, bool) {
    match render_mjml(&mjml_source(message, sender)) {
        Ok(html) => (html, true),
        Err(_) => (simple_html(message, sender), false),
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
    let text = plain_body(message, envelope.from_name);
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
