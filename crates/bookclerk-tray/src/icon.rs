//! Decode the packaged tray PNG into ksni ARGB32.

#[cfg(target_os = "linux")]
pub fn tray_icon() -> ksni::Icon {
    decode_png(include_bytes!("../tray-icon.png"))
}

#[cfg(target_os = "linux")]
fn decode_png(bytes: &[u8]) -> ksni::Icon {
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
    let mut data = rgba;
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
}
