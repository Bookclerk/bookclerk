//! Frame decoder and encoder tests.

use super::*;

/// Decodes `bytes` pushed in one go into every complete frame.
fn decode_all(bytes: &[u8]) -> Result<Vec<ClientFrame>, FrameError> {
    let mut decoder = FrameDecoder::new();
    decoder.push(bytes);
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame()? {
        frames.push(frame);
    }
    Ok(frames)
}

#[test]
fn data_and_start_frames_round_trip() {
    let frames = vec![
        ClientFrame::Data(b"EHLO x\r\n".to_vec()),
        ClientFrame::StartTls,
        ClientFrame::Data(vec![7; MAX_FRAME_PAYLOAD]),
    ];
    let wire: Vec<u8> = frames.iter().flat_map(encode_client_frame).collect();
    assert_eq!(decode_all(&wire).unwrap(), frames);
}

#[test]
fn frames_split_across_reads_wait_for_the_rest() {
    let wire = encode_client_frame(&ClientFrame::Data(b"hello".to_vec()));
    let mut decoder = FrameDecoder::new();
    for (i, byte) in wire.iter().enumerate() {
        assert_eq!(decoder.next_frame().unwrap(), None, "early frame at {i}");
        decoder.push(&[*byte]);
    }
    assert_eq!(
        decoder.next_frame().unwrap(),
        Some(ClientFrame::Data(b"hello".to_vec()))
    );
    assert_eq!(decoder.buffered(), 0);
}

#[test]
fn malformed_headers_are_refused_before_the_payload_arrives() {
    assert_eq!(decode_all(&[0, 0, 0]), Err(FrameError::BadDataLength(0)));
    let too_long = u16::try_from(MAX_FRAME_PAYLOAD + 1).unwrap().to_be_bytes();
    assert_eq!(
        decode_all(&[0, too_long[0], too_long[1]]),
        Err(FrameError::BadDataLength(MAX_FRAME_PAYLOAD + 1))
    );
    assert_eq!(
        decode_all(&[1, 0, 1, 9]),
        Err(FrameError::StartTlsPayload(1))
    );
    assert_eq!(decode_all(&[2, 0, 0]), Err(FrameError::UnknownKind(2)));
    assert_eq!(decode_all(&[0xff]), Ok(Vec::new()));
}

#[test]
fn server_bytes_become_bounded_data_frames() {
    assert!(encode_data(b"").is_empty());
    let bytes = vec![1_u8; MAX_FRAME_PAYLOAD * 2 + 5];
    let wire = encode_data(&bytes);
    assert_eq!(wire.len(), bytes.len() + 3 * FRAME_HEADER_BYTES);
    assert_eq!(&wire[..3], &[0, 0x40, 0x00]);
    let tail = &wire[wire.len() - 8..];
    assert_eq!(&tail[..3], &[0, 0, 5]);
    // Server frames share the client data layout, so the decoder reads them.
    let decoded = decode_all(&wire).unwrap();
    let joined: Vec<u8> = decoded
        .into_iter()
        .flat_map(|frame| match frame {
            ClientFrame::Data(bytes) => bytes,
            ClientFrame::StartTls => panic!("unexpected start"),
        })
        .collect();
    assert_eq!(joined, bytes);
}

#[test]
fn tls_outcome_frames_are_fixed_and_bounded() {
    assert_eq!(encode_tls_ready(), vec![1, 0, 0]);
    assert_eq!(encode_tls_failed("no"), vec![2, 0, 2, b'n', b'o']);
    let long = "é".repeat(MAX_FAILURE_REASON_BYTES);
    let frame = encode_tls_failed(&long);
    let len = usize::from(u16::from_be_bytes([frame[1], frame[2]]));
    assert!(len <= MAX_FAILURE_REASON_BYTES);
    assert_eq!(frame.len(), FRAME_HEADER_BYTES + len);
    assert!(std::str::from_utf8(&frame[3..]).is_ok());
}

#[test]
fn only_a_named_host_before_the_suffix_asks_for_framing() {
    assert_eq!(
        strip_starttls_suffix("smtp.example.com.starttls.bookclerk.invalid"),
        Some("smtp.example.com")
    );
    assert_eq!(
        strip_starttls_suffix("10.0.0.5.starttls.bookclerk.invalid"),
        Some("10.0.0.5")
    );
    assert_eq!(strip_starttls_suffix(".starttls.bookclerk.invalid"), None);
    assert_eq!(strip_starttls_suffix("a..starttls.bookclerk.invalid"), None);
    assert_eq!(strip_starttls_suffix("smtp.example.com"), None);
    assert_eq!(strip_starttls_suffix("starttls.bookclerk.invalid"), None);
}
