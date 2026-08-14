//! Terminal QR rendering for headless OAuth.

use qrcode::render::unicode;
use qrcode::QrCode;

use crate::error::{AudibleError, Result};

/// How to print the login QR in a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QrRenderMode {
    /// Dense Unicode half-blocks (default; works in most modern terminals).
    #[default]
    Unicode,
    /// ASCII `#` / space art for limited terminals.
    Ascii,
}

/// Render `url` as a terminal QR code string (plus the raw URL).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn render_login_qr(url: &str, mode: QrRenderMode) -> Result<String> {
    let code = QrCode::new(url.as_bytes())
        .map_err(|err| AudibleError::Auth(format!("failed to encode QR: {err}")))?;

    let art = match mode {
        QrRenderMode::Unicode => code
            .render::<unicode::Dense1x2>()
            .dark_color(unicode::Dense1x2::Dark)
            .light_color(unicode::Dense1x2::Light)
            .build(),
        QrRenderMode::Ascii => {
            // Simple ASCII: module true => '#', false => ' '
            let width = code.width();
            let mut lines = Vec::with_capacity(width + 2);
            let border = "#".repeat(width + 4);
            lines.push(border.clone());
            for y in 0..width {
                let mut row = String::from("# ");
                for x in 0..width {
                    row.push(if code[(x, y)] == qrcode::Color::Dark {
                        '#'
                    } else {
                        ' '
                    });
                }
                row.push_str(" #");
                lines.push(row);
            }
            lines.push(border);
            lines.join("\n")
        }
    };

    Ok(format!(
        "{art}\n\nOpen or scan this URL to continue login:\n{url}\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_unicode_and_ascii() {
        let url = "https://www.amazon.com/ap/signin?openid...";
        let uni = render_login_qr(url, QrRenderMode::Unicode).unwrap();
        assert!(uni.contains(url));
        let ascii = render_login_qr(url, QrRenderMode::Ascii).unwrap();
        assert!(ascii.contains('#'));
        assert!(ascii.contains(url));
    }
}
