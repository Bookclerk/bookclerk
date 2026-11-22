//! `GET /api/status/media`: what this build's media worker reads and writes.
//!
//! The answer is [`bookclerk_media::capabilities::capabilities`], the same
//! report the acquire pipeline consults before handing the worker a
//! library's copy, so a settings page can offer only the outputs this
//! build writes.

use axum::Json;
use bookclerk_media::capabilities::{capabilities, Capabilities, OutputEncoding};
use serde::Serialize;

/// One encoding the media worker writes.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct OutputEncodingJson {
    /// The output format id that selects it (`m4b`, `mp3` or `opus`).
    #[schema(example = "opus")]
    pub name: &'static str,
    /// True when settings offer it only under their advanced options,
    /// because fewer players open it than M4B and MP3.
    #[schema(example = true)]
    pub advanced: bool,
}

/// The codecs, containers and encodings this build's media worker handles.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct MediaCapabilitiesJson {
    /// Audio codecs the worker decodes (`aac`, `alac`, `flac`, `mp3`,
    /// `opus`, `vorbis`, and `eac3` in builds with the FFmpeg decoders).
    #[schema(example = json!(["aac", "alac", "flac", "mp3", "opus", "vorbis"]))]
    pub decodes: Vec<&'static str>,
    /// Media types of files the worker opens, compared without case or
    /// parameters. Builds with the FFmpeg decoders add WMA, raw AC-3 and
    /// E-AC-3, and AMR types.
    #[schema(example = json!(["audio/mp4", "audio/mpeg", "audio/flac", "audio/ogg"]))]
    pub reads: Vec<&'static str>,
    /// Encodings the worker writes.
    #[schema(example = json!([
        {"name": "m4b", "advanced": false},
        {"name": "mp3", "advanced": false},
        {"name": "opus", "advanced": true}
    ]))]
    pub encodes: Vec<OutputEncodingJson>,
}

impl From<Capabilities> for MediaCapabilitiesJson {
    fn from(report: Capabilities) -> Self {
        Self {
            decodes: report.decodes.iter().map(|codec| codec.as_str()).collect(),
            reads: report.reads.to_vec(),
            encodes: report
                .encodes
                .iter()
                .map(|encoding: &OutputEncoding| OutputEncodingJson {
                    name: encoding.name(),
                    advanced: encoding.is_advanced(),
                })
                .collect(),
        }
    }
}

/// Returns the audio codecs, file types and output encodings this build's
/// media worker handles.
#[utoipa::path(
    get,
    path = "/api/status/media",
    responses(
        (status = 200, description = "What the media worker reads and writes.", body = MediaCapabilitiesJson),
    ),
)]
pub(crate) async fn media_capabilities() -> Json<MediaCapabilitiesJson> {
    Json(capabilities().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_report_names_codecs_types_and_encodings() {
        let Json(report) = media_capabilities().await;
        assert!(report.decodes.contains(&"opus"));
        assert!(report.reads.contains(&"audio/ogg"));
        let opus = report
            .encodes
            .iter()
            .find(|e| e.name == "opus")
            .map(|e| e.advanced);
        assert_eq!(opus, Some(true));
        let value = serde_json::to_value(&report).unwrap_or_default();
        assert_eq!(value["encodes"][0]["name"], "m4b");
    }
}
