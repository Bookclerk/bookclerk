//! Which origins webhook URLs are handed out on.

use super::*;

#[test]
fn only_public_https_origins_get_webhook_urls() {
    let auto = WebhooksExposure::Auto;
    for (origin, expected) in [
        (
            "https://library.example.org",
            Some("https://library.example.org"),
        ),
        (
            "https://library.example.org/",
            Some("https://library.example.org"),
        ),
        (
            "https://books.example.net:8443",
            Some("https://books.example.net:8443"),
        ),
        ("https://203.0.113.9", None),
        ("https://8.8.8.8", Some("https://8.8.8.8")),
        (
            "https://[2606:4700::1111]",
            Some("https://[2606:4700::1111]"),
        ),
        ("http://library.example.org", None),
        ("https://localhost:8787", None),
        ("https://nas.local", None),
        ("https://nas", None),
        ("https://library.home.arpa", None),
        ("https://192.168.1.10", None),
        ("https://10.0.0.1", None),
        ("https://100.64.1.1", None),
        ("https://127.0.0.1", None),
        ("https://[::1]", None),
        ("https://[fd00::1]", None),
        ("https://[fe80::1]", None),
        ("https://[::ffff:192.168.1.1]", None),
        ("https://library.example.org/sub", None),
        ("https://user@library.example.org", None),
        ("", None),
    ] {
        assert_eq!(
            webhook_origin(Some(origin), auto).as_deref(),
            expected,
            "{origin}"
        );
    }
    assert_eq!(webhook_origin(None, auto), None);
    assert_eq!(
        webhook_origin(Some("https://library.example.org"), WebhooksExposure::Off),
        None
    );
}
