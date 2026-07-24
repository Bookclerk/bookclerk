//! Default content-source registry (Audible, Libro.fm, GraphicAudio, Chirp).

use std::sync::Arc;

use libation_audible::AudibleSource;
use libation_chirp::ChirpSource;
use libation_config::Config;
use libation_graphicaudio::GraphicAudioSource;
use libation_libro::LibroSource;
use libation_source::SourceRegistry;

/// Build a registry with enabled first-party content sources from config.
#[must_use]
pub fn default_registry(config: &Config) -> SourceRegistry {
    let mut r = SourceRegistry::new();
    if config.sources.is_enabled("audible") {
        r.register(Arc::new(AudibleSource));
    }
    if config.sources.is_enabled("libro") {
        r.register(Arc::new(LibroSource::new()));
    }
    if config.sources.is_enabled("graphicaudio") {
        r.register(Arc::new(
            GraphicAudioSource::new().with_access(config.sources.graphicaudio.access),
        ));
    }
    if config.sources.is_enabled("chirp") {
        r.register(Arc::new(ChirpSource::new()));
    }
    r
}
