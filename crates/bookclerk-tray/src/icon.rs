//! Decode the packaged tray PNG into platform icon formats.

#[cfg(target_os = "linux")]
/// Decodes the packaged tray PNG into a StatusNotifierItem ARGB icon.
///
/// # Returns
///
/// `ksni::Icon` with width/height and ARGB pixel bytes.
#[must_use]
pub fn tray_icon() -> ksni::Icon {
    let (width, height, mut data) = decode_png_rgba(include_bytes!("../tray-icon.png"));
    for pixel in data.chunks_exact_mut(4) {
        // RGBA → ARGB (ksni / StatusNotifierItem)
        pixel.rotate_right(1);
    }
    ksni::Icon {
        width: width as i32,
        height: height as i32,
        data,
    }
}

#[cfg(any(windows, target_os = "macos"))]
/// Decodes the packaged tray PNG into a `tray-icon` RGBA icon.
///
/// # Returns
///
/// Platform tray icon built from RGBA pixels.
///
/// # Errors
///
/// Returns an error when the PNG cannot be converted into a `tray_icon::Icon`.
pub fn tray_icon_rgba() -> anyhow::Result<tray_icon::Icon> {
    let (width, height, data) = decode_png_rgba(include_bytes!("../tray-icon.png"));
    tray_icon::Icon::from_rgba(data, width, height)
        .map_err(|err| anyhow::anyhow!("tray icon: {err}"))
}

/// Decodes a packaged PNG into width, height, and tightly packed RGBA pixels.
fn decode_png_rgba(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("tray png header");
    let mut buf = vec![
        0;
        reader
            .output_buffer_size()
            .expect("tray png dimensions fit memory")
    ];
    let info = reader.next_frame(&mut buf).expect("tray png frame");
    let width = info.width;
    let height = info.height;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => {
            let rgb = &buf[..info.buffer_size()];
            let mut out = Vec::with_capacity(width as usize * height as usize * 4);
            for pixel in rgb.chunks_exact(3) {
                out.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            out
        }
        other => panic!("unsupported tray png color type: {other:?}"),
    };
    (width, height, rgba)
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn packaged_tray_icon_decodes() {
        let icon = super::tray_icon();
        assert!(icon.width > 0);
        assert!(icon.height > 0);
        assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn packaged_tray_icon_rgba_decodes() {
        let icon = super::tray_icon_rgba().expect("decode");
        let _ = icon;
    }
}
