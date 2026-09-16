//! Re-export install `plugin.toml` types from `bookclerk-plugin-manifest`.

pub use bookclerk_plugin_manifest::{
    embedded_logo_api_path, entrypoint_family, logo_content_type, validate_logo,
    BindingCapabilities, CapabilitiesManifest, DatabaseBindingManifest, Entrypoint, EventConsumer,
    EventProducer, EventsManifest, JailNetworkNeed, LogoKind, ModuleSpec, NamedBinding,
    NetworkCapabilities, NetworkMode, PluginFamily, PluginManifest, PluginRuntimeKind,
    TriggersManifest, WorkerdLimits, WorkerdRuntimeManifest, ALL_ENTRYPOINTS,
    MAX_EMBEDDED_LOGO_BYTES,
};
