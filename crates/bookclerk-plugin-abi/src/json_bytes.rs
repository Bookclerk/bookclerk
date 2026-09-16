//! Serde helpers projecting Cap'n Proto `Data` fields as base64 text in JSON.
//!
//! The typed ABI travels as Cap'n Proto; JSON projections exist only for
//! transport-private bridges (workerd HTTP bridge, native broker), operator
//! diagnostics, and tests. Bytes serialize as standard base64 (padded) and
//! deserialize from either base64 text or a JSON number array, so hand-written
//! fixtures stay readable.

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Standard padded base64 alphabet.
fn engine() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// Accepted JSON encodings of a byte string.
#[derive(Deserialize)]
#[serde(untagged)]
enum BytesRepr {
    /// Base64 text (canonical projection).
    Text(String),
    /// JSON number array (hand-written fixtures).
    Array(Vec<u8>),
}

/// Decodes either accepted representation.
///
/// # Errors
///
/// Returns when base64 text is not valid.
fn decode(repr: BytesRepr) -> Result<Vec<u8>, String> {
    match repr {
        BytesRepr::Array(bytes) => Ok(bytes),
        BytesRepr::Text(text) => {
            if text.is_empty() {
                return Ok(Vec::new());
            }
            engine()
                .decode(text.as_bytes())
                .map_err(|err| format!("invalid base64 bytes: {err}"))
        }
    }
}

/// `Vec<u8>` as base64 text.
pub mod b64 {
    use super::{decode, engine, BytesRepr, Deserialize, Deserializer, Serialize, Serializer};
    use base64::Engine as _;
    use serde::de::Error as _;

    /// Serializes bytes as base64 text.
    ///
    /// # Errors
    ///
    /// Propagates serializer failures.
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        engine().encode(bytes).serialize(serializer)
    }

    /// Deserializes base64 text (or a byte array) into bytes.
    ///
    /// # Errors
    ///
    /// Returns when the text is not valid base64.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let repr = BytesRepr::deserialize(deserializer)?;
        decode(repr).map_err(D::Error::custom)
    }
}

/// `Option<Vec<u8>>` as base64 text; `null`/absent is `None`.
pub mod opt_b64 {
    use super::{decode, engine, BytesRepr, Deserialize, Deserializer, Serialize, Serializer};
    use base64::Engine as _;
    use serde::de::Error as _;

    /// Serializes optional bytes as base64 text or `null`.
    ///
    /// # Errors
    ///
    /// Propagates serializer failures.
    pub fn serialize<S: Serializer>(
        bytes: &Option<Vec<u8>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(bytes) => engine().encode(bytes).serialize(serializer),
            None => serializer.serialize_none(),
        }
    }

    /// Deserializes base64 text, a byte array, or `null`.
    ///
    /// # Errors
    ///
    /// Returns when the text is not valid base64.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        let repr = Option::<BytesRepr>::deserialize(deserializer)?;
        repr.map(decode).transpose().map_err(D::Error::custom)
    }
}

/// Encodes bytes as standard base64 text (bridge JSON projection).
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    engine().encode(bytes)
}

/// Decodes standard base64 text.
///
/// # Errors
///
/// Returns when the text is not valid base64.
pub fn decode_text(text: &str) -> Result<Vec<u8>, String> {
    decode(BytesRepr::Text(text.to_owned()))
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Blob {
        #[serde(with = "b64")]
        data: Vec<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_b64")]
        maybe: Option<Vec<u8>>,
    }

    #[test]
    fn roundtrip_base64_text() {
        let blob = Blob {
            data: vec![1, 2, 3, 255],
            maybe: Some(b"hi".to_vec()),
        };
        let json = serde_json::to_string(&blob).unwrap();
        assert_eq!(json, r#"{"data":"AQID/w==","maybe":"aGk="}"#);
        assert_eq!(serde_json::from_str::<Blob>(&json).unwrap(), blob);
    }

    #[test]
    fn accepts_number_arrays_and_absent_optional() {
        let blob: Blob = serde_json::from_str(r#"{"data":[104,105]}"#).unwrap();
        assert_eq!(blob.data, b"hi");
        assert_eq!(blob.maybe, None);
        let blob: Blob = serde_json::from_str(r#"{"data":"","maybe":null}"#).unwrap();
        assert!(blob.data.is_empty());
        assert_eq!(blob.maybe, None);
    }

    #[test]
    fn rejects_invalid_base64() {
        assert!(serde_json::from_str::<Blob>(r#"{"data":"%%%"}"#).is_err());
    }
}
