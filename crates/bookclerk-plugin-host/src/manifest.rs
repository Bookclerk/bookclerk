//! Re-export install `plugin.toml` types from `bookclerk-plugin-manifest`.

pub use bookclerk_plugin_manifest::{
    embedded_logo_api_path, logo_content_type, validate_logo, BindingCapabilities,
    CapabilitiesManifest, JailNetworkNeed, LogoKind, MethodCapabilities, ModuleSpec,
    NetworkCapabilities, NetworkMode, PluginKind, PluginManifest, PluginRuntimeKind, WorkerdLimits,
    WorkerdRuntimeManifest, MAX_EMBEDDED_LOGO_BYTES,
};
