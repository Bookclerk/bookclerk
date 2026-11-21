//! Shared oracle for the `notify_raise` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! A raise is what a plugin sends on its `NOTIFY` binding. Byte 0 modulo 2
//! picks the form the rest is in: the JSON body the workerd bridge's
//! `/notify/raise` route reads (0), or an unpacked `NotifyRaiseParams`
//! message, the form a native guest's raise crosses the plugin connection
//! in (1). The invariants:
//!
//! - decoding and checking never panic;
//! - [`check_raise_params`] accepts the raise exactly when the independent
//!   restatement below finds every rule kept: a local type id, at most 25
//!   fields with non-blank one-line names and capped values, a well-formed
//!   patron id, a link path that starts with exactly one `/` and has no
//!   space, backslash or control character, a one-line group key, and an
//!   alert key exactly when the alert action is not `none`;
//! - an accepted raise round-trips through the codec and is accepted again;
//! - the host's patron-scope parse ([`scoped_patron_id`]) names a user
//!   exactly when the invocation has an account and the patron id is a
//!   canonical positive decimal that fits an `i64`, and then the user whose
//!   decimal form is the id, for every invocation account tried (none, and
//!   two different ones). The holder lookup that follows is a library read,
//!   tested with the store.

use bookclerk_plugin_abi::notifications::{
    check_raise_params, decode_notify_raise_params_bytes, encode_notify_raise_params_bytes,
    scoped_patron_id,
};
use bookclerk_plugin_abi::{NotificationAlertAction, NotifyRaiseParams};

/// Largest input worth decoding; the bridge caps a raise body at 128 KiB.
pub const MAX_INPUT_BYTES: usize = 128 * 1024;

/// Title and field-name cap in bytes (`maxNotificationTitleBytes`).
const TITLE: usize = 256;
/// Field value cap in bytes (`maxNotificationFieldBytes`).
const FIELD: usize = 1_024;
/// Field count cap (`maxNotificationFields`).
const FIELDS: usize = 25;

/// Characters that break a line or reorder text without being control
/// characters, restated as a list: line and paragraph separators, the
/// Arabic letter mark, LRM / RLM, the embeddings and overrides, and the
/// isolates.
fn layout(c: char) -> bool {
    const LIST: [u32; 14] = [
        0x2028, 0x2029, 0x061C, 0x200E, 0x200F, 0x202A, 0x202B, 0x202C, 0x202D, 0x202E, 0x2066,
        0x2067, 0x2068, 0x2069,
    ];
    LIST.contains(&u32::from(c))
}

/// Runs the invariants on one input.
///
/// # Panics
///
/// When an invariant breaks.
pub fn check(data: &[u8]) {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Some((selector, rest)) = data.split_first() else {
        return;
    };
    let params: NotifyRaiseParams = if selector % 2 == 0 {
        match serde_json::from_slice(rest) {
            Ok(params) => params,
            Err(_) => return,
        }
    } else {
        match decode_notify_raise_params_bytes(rest) {
            Ok(params) => params,
            Err(_) => return,
        }
    };
    let accepted = check_raise_params(&params).is_ok();
    assert_eq!(accepted, expected(&params), "raise {params:?}");
    let patron = params.patron_id.as_deref().unwrap_or("");
    for (index, account) in ["", "acct-a", "acct-b"].into_iter().enumerate() {
        let scoped = scoped_patron_id(account, patron);
        assert_eq!(
            scoped,
            expected_patron(account, patron),
            "patron {patron:?} in account {index}"
        );
        if let Some(id) = scoped {
            assert_eq!(
                id.to_string(),
                patron,
                "patron {patron:?} named another way"
            );
        }
    }
    if accepted {
        let bytes = encode_notify_raise_params_bytes(&params)
            .unwrap_or_else(|err| panic!("encode an accepted raise: {err}"));
        let again = decode_notify_raise_params_bytes(&bytes)
            .unwrap_or_else(|err| panic!("decode an encoded raise: {err}"));
        assert!(check_raise_params(&again).is_ok(), "round trip refused");
        assert_eq!(again.notification_type, params.notification_type);
        assert_eq!(again.fields, params.fields);
        assert_eq!(again.alert_action, params.alert_action);
        // Absent and empty optional texts are the same on the wire.
        let norm = |v: &Option<String>| v.clone().filter(|s| !s.is_empty());
        assert_eq!(norm(&again.patron_id), norm(&params.patron_id));
        assert_eq!(norm(&again.link_path), norm(&params.link_path));
        assert_eq!(norm(&again.group_key), norm(&params.group_key));
        assert_eq!(norm(&again.alert_key), norm(&params.alert_key));
    }
}

/// No control character at all, unless `multiline` allows tab and breaks.
fn clean(value: &str, multiline: bool) -> bool {
    value.chars().all(|c| {
        !(c.is_control() || layout(c))
            || (multiline && matches!(c, '\t' | '\n' | '\r' | '\u{2028}' | '\u{2029}'))
    })
}

/// A non-blank single line within `max` bytes.
fn line(value: &str, max: usize) -> bool {
    value.len() <= max && !value.trim().is_empty() && clean(value, false)
}

/// The independent restatement of every raise rule.
fn expected(params: &NotifyRaiseParams) -> bool {
    let id = params.notification_type.as_bytes();
    let type_ok = !id.is_empty()
        && id.len() <= 64
        && id[0].is_ascii_lowercase()
        && id
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_');
    let fields_ok = params.fields.len() <= FIELDS
        && params
            .fields
            .iter()
            .all(|f| line(&f.name, TITLE) && f.value.len() <= FIELD && clean(&f.value, true));
    let present = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(str::to_owned);
    let patron_ok = present(&params.patron_id)
        .is_none_or(|p| p.len() <= 128 && p.bytes().all(|b| (0x21..=0x7e).contains(&b)));
    let path_ok = present(&params.link_path).is_none_or(|p| {
        p.len() <= FIELD
            && p.starts_with('/')
            && !p.starts_with("//")
            && p.chars()
                .all(|c| !c.is_control() && !c.is_whitespace() && c != '\\')
    });
    let group_ok = present(&params.group_key).is_none_or(|g| line(&g, FIELD));
    let alert_ok = match (params.alert_action, present(&params.alert_key)) {
        (NotificationAlertAction::None, None) => true,
        (NotificationAlertAction::None, Some(_)) => false,
        (NotificationAlertAction::Trigger | NotificationAlertAction::Resolve, key) => {
            key.is_some_and(|k| line(&k, FIELD))
        }
    };
    type_ok && fields_ok && patron_ok && path_ok && group_ok && alert_ok
}

/// The independent restatement of the scope parse: an account, then a
/// non-empty run of ASCII digits with no leading zero whose value, summed
/// digit by digit, stays within `i64`.
fn expected_patron(account: &str, patron: &str) -> Option<i64> {
    if account.is_empty() || patron.is_empty() || patron.as_bytes()[0] == b'0' {
        return None;
    }
    let mut value: i64 = 0;
    for b in patron.bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(i64::from(b - b'0'))?;
    }
    Some(value)
}
