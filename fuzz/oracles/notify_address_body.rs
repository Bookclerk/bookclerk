//! Shared oracle of the `notify_address_body` fuzz target and its replay
//! test: the `address` object of a patron's
//! `POST /api/me/notifications/destinations` body, read by
//! `bookclerk-plugin-abi::notifications::AddressFields` exactly as the
//! route's JSON extractor reads it.
//!
//! The independent restatement reads the same bytes as a list of entries
//! that keeps repeats, and accepts exactly a JSON object whose values are
//! all strings and whose keys are all different.

use std::collections::BTreeMap;

use bookclerk_plugin_abi::notifications::AddressFields;

/// Every entry of one JSON object, repeats included.
struct Entries(Vec<(String, serde_json::Value)>);

impl<'de> serde::Deserialize<'de> for Entries {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        /// Collects every entry in order.
        struct Collect;

        impl<'de> serde::de::Visitor<'de> for Collect {
            type Value = Entries;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(entry) = map.next_entry::<String, serde_json::Value>()? {
                    out.push(entry);
                }
                Ok(Entries(out))
            }
        }

        de.deserialize_map(Collect)
    }
}

/// What the restatement expects: the fields when the bytes are one object
/// of distinct string-valued keys, otherwise `None`.
fn expected(data: &[u8]) -> Option<BTreeMap<String, String>> {
    let Entries(entries) = serde_json::from_slice::<Entries>(data).ok()?;
    let mut fields = BTreeMap::new();
    for (key, value) in entries {
        let serde_json::Value::String(value) = value else {
            return None;
        };
        if fields.insert(key, value).is_some() {
            return None;
        }
    }
    Some(fields)
}

/// Checks one input; returns whether it was accepted.
///
/// # Panics
///
/// When the reader and the restatement disagree, or an accepted address
/// does not read back the same after writing it out again.
#[allow(clippy::expect_used, clippy::panic)]
pub fn check(data: &[u8]) -> bool {
    let read = serde_json::from_slice::<AddressFields>(data).ok();
    let want = expected(data);
    assert_eq!(
        read.as_ref().map(|fields| &fields.0),
        want.as_ref(),
        "AddressFields and the restatement disagree"
    );
    let Some(fields) = read else {
        return false;
    };
    let text = serde_json::to_string(&fields.0).expect("a map of strings serialises");
    let again = serde_json::from_str::<AddressFields>(&text).expect("written text reads back");
    assert_eq!(again, fields, "an accepted address reads back the same");
    true
}
