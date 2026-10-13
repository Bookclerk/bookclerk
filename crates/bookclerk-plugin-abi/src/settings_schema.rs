//! Operator-facing plugin settings schema (`[[settings]]` in `plugin.toml`).
//!
//! A plugin declares the keys an operator may set on one of its instances,
//! their types, and their defaults. The host validates instance documents
//! against this schema and renders it in Settings; it hard-codes nothing per
//! plugin. The same struct is the `plugin.toml` TOML shape and the JSON body
//! pinned in the library database at enrollment.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::generated::{ConfigOption, ConfigOptionValue};

/// Schema revision of [`SettingsSchema`] as pinned in the database.
pub const SETTINGS_SCHEMA_VERSION: u32 = 1;

/// Maximum `[[settings]]` entries one manifest may declare.
pub const MAX_SETTINGS: usize = 32;

/// Maximum enum values one setting may declare.
pub const MAX_SETTING_VALUES: usize = 64;

/// Maximum UTF-8 size of one string setting value.
pub const MAX_SETTING_STRING_BYTES: usize = 1024;

/// Value type of one setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingKind {
    /// Free-form UTF-8 string (at most [`MAX_SETTING_STRING_BYTES`]).
    String,
    /// Boolean.
    Bool,
    /// Signed 64-bit integer.
    Integer,
    /// One of the declared [`SettingSpec::values`] ids.
    Enum,
    /// Sealed secret: delivered in `SECRETS`, never stored in a document.
    Secret,
}

impl SettingKind {
    /// Wire / UI name (`string`, `bool`, `integer`, `enum`, `secret`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Bool => "bool",
            Self::Integer => "integer",
            Self::Enum => "enum",
            Self::Secret => "secret",
        }
    }
}

/// One selectable value of an [`SettingKind::Enum`] setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingValueSpec {
    /// Stored value.
    pub id: String,
    /// Operator-facing label; defaults to `id`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
}

/// One `[[settings]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingSpec {
    /// Setting key (`^[a-z][a-z0-9_]{0,63}$`), unique within the plugin.
    pub key: String,
    /// Operator-facing label; defaults to `key`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// Value type.
    #[serde(rename = "type")]
    pub kind: SettingKind,
    /// Allowed values for [`SettingKind::Enum`]; empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<SettingValueSpec>,
    /// Default applied when the instance document omits the key. Not allowed
    /// for secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// When true, a spawn without a value (document or default) fails.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// Optional help text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl SettingSpec {
    /// Label for UI, falling back to the key.
    #[must_use]
    pub fn label_or_key(&self) -> &str {
        if self.label.is_empty() {
            &self.key
        } else {
            &self.label
        }
    }

    /// True for [`SettingKind::Secret`].
    #[must_use]
    pub fn is_secret(&self) -> bool {
        self.kind == SettingKind::Secret
    }

    /// Checks `value` against this entry's kind.
    ///
    /// # Errors
    ///
    /// Returns a message naming the key when the value does not fit.
    pub fn check_value(&self, value: &Value) -> Result<(), String> {
        let key = &self.key;
        match self.kind {
            SettingKind::Secret => Err(format!(
                "setting `{key}` is a secret; store it through the secrets binding"
            )),
            SettingKind::Bool => value
                .as_bool()
                .map(|_| ())
                .ok_or_else(|| format!("setting `{key}` must be a boolean")),
            SettingKind::Integer => value
                .as_i64()
                .map(|_| ())
                .ok_or_else(|| format!("setting `{key}` must be an integer")),
            SettingKind::String => match value.as_str() {
                Some(text) if text.len() <= MAX_SETTING_STRING_BYTES && !text.contains('\0') => {
                    Ok(())
                }
                Some(_) => Err(format!(
                    "setting `{key}` must be at most {MAX_SETTING_STRING_BYTES} bytes"
                )),
                None => Err(format!("setting `{key}` must be a string")),
            },
            SettingKind::Enum => match value.as_str() {
                Some(text) if self.values.iter().any(|v| v.id == text) => Ok(()),
                _ => Err(format!(
                    "setting `{key}` must be one of: {}",
                    self.values
                        .iter()
                        .map(|v| v.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            },
        }
    }
}

/// Grammar for setting keys and secret names: `^[a-z][a-z0-9_]{0,63}$`.
#[must_use]
pub fn is_valid_setting_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && key.len() <= 64
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

/// The ordered `[[settings]]` list of one plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsSchema {
    /// Entries in declaration order.
    #[serde(default)]
    pub settings: Vec<SettingSpec>,
}

impl SettingsSchema {
    /// Wraps a declared list without validating it.
    #[must_use]
    pub fn new(settings: Vec<SettingSpec>) -> Self {
        Self { settings }
    }

    /// Validates the declaration itself.
    ///
    /// `config_binding` / `secrets_binding` say whether the manifest declares
    /// `[vars]` / `[secrets]`; non-secret entries need the former and secret
    /// entries the latter, because that is how values reach the guest.
    ///
    /// # Errors
    ///
    /// Returns a message describing the first failed rule.
    pub fn validate(&self, config_binding: bool, secrets_binding: bool) -> Result<(), String> {
        if self.settings.len() > MAX_SETTINGS {
            return Err(format!("at most {MAX_SETTINGS} settings may be declared"));
        }
        let mut seen = std::collections::HashSet::new();
        for spec in &self.settings {
            let key = &spec.key;
            if !is_valid_setting_key(key) {
                return Err(format!(
                    "settings key `{key}` must match ^[a-z][a-z0-9_]{{0,63}}$"
                ));
            }
            if !seen.insert(key.as_str()) {
                return Err(format!("settings key `{key}` is duplicated"));
            }
            if spec.kind == SettingKind::Enum {
                if spec.values.is_empty() {
                    return Err(format!("enum setting `{key}` must declare `values`"));
                }
                if spec.values.len() > MAX_SETTING_VALUES {
                    return Err(format!(
                        "enum setting `{key}` declares more than {MAX_SETTING_VALUES} values"
                    ));
                }
                let mut ids = std::collections::HashSet::new();
                for value in &spec.values {
                    if value.id.is_empty() || value.id.len() > MAX_SETTING_STRING_BYTES {
                        return Err(format!("enum setting `{key}` has an invalid value id"));
                    }
                    if !ids.insert(value.id.as_str()) {
                        return Err(format!(
                            "enum setting `{key}` value `{}` is duplicated",
                            value.id
                        ));
                    }
                }
            } else if !spec.values.is_empty() {
                return Err(format!(
                    "setting `{key}` declares `values` but is not an enum"
                ));
            }
            if spec.kind == SettingKind::Secret {
                if spec.default.is_some() {
                    return Err(format!("secret setting `{key}` may not declare a default"));
                }
                if !secrets_binding {
                    return Err(format!(
                        "secret setting `{key}` requires the `[secrets]` binding"
                    ));
                }
            } else {
                if !config_binding {
                    return Err(format!("setting `{key}` requires the `[vars]` binding"));
                }
                if let Some(default) = &spec.default {
                    spec.check_value(default)
                        .map_err(|err| format!("default for {err}"))?;
                }
            }
        }
        Ok(())
    }

    /// Entry for `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&SettingSpec> {
        self.settings.iter().find(|spec| spec.key == key)
    }

    /// True when no entries are declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.settings.is_empty()
    }

    /// Non-secret entries, in order.
    pub fn config_entries(&self) -> impl Iterator<Item = &SettingSpec> {
        self.settings.iter().filter(|spec| !spec.is_secret())
    }

    /// Secret entries, in order.
    pub fn secret_entries(&self) -> impl Iterator<Item = &SettingSpec> {
        self.settings.iter().filter(|spec| spec.is_secret())
    }

    /// Defaults for non-secret entries that declare one.
    #[must_use]
    pub fn defaults(&self) -> BTreeMap<String, Value> {
        self.config_entries()
            .filter_map(|spec| spec.default.clone().map(|v| (spec.key.clone(), v)))
            .collect()
    }

    /// `describe().configOptions` projection: every non-secret entry, with
    /// enum values listed and other kinds as free-form.
    #[must_use]
    pub fn config_options(&self) -> Vec<ConfigOption> {
        self.config_entries()
            .map(|spec| ConfigOption {
                key: spec.key.clone(),
                label: spec.label_or_key().to_string(),
                values: spec
                    .values
                    .iter()
                    .map(|value| ConfigOptionValue {
                        id: value.id.clone(),
                        label: if value.label.is_empty() {
                            value.id.clone()
                        } else {
                            value.label.clone()
                        },
                    })
                    .collect(),
            })
            .collect()
    }

    /// Checks one document setting against the schema.
    ///
    /// # Errors
    ///
    /// Unknown keys, secrets given as plain values, and type mismatches are
    /// rejected with a message naming the key.
    pub fn check_setting(&self, key: &str, value: &Value) -> Result<(), String> {
        match self.get(key) {
            Some(spec) => spec.check_value(value),
            None => Err(format!("setting `{key}` is not declared by the plugin")),
        }
    }

    /// Checks one secret ref key against the schema.
    ///
    /// # Errors
    ///
    /// Returns a message when `key` is not a declared secret.
    pub fn check_secret_key(&self, key: &str) -> Result<(), String> {
        match self.get(key) {
            Some(spec) if spec.is_secret() => Ok(()),
            Some(_) => Err(format!("setting `{key}` is not a secret")),
            None => Err(format!("secret `{key}` is not declared by the plugin")),
        }
    }

    /// Keys of `secret` entries marked `required`.
    pub fn required_secret_keys(&self) -> impl Iterator<Item = &str> {
        self.secret_entries()
            .filter(|spec| spec.required)
            .map(|spec| spec.key.as_str())
    }

    /// Merges `settings` over the schema defaults and reports missing
    /// required keys.
    ///
    /// Keys the schema does not declare are dropped and returned in
    /// `dropped`; declared keys whose value no longer fits the entry's kind
    /// (a type or enum change after an upgrade) are left out of `config` and
    /// returned in `invalid`, so the default applies instead. A required key
    /// with neither a usable value nor a default is `missing`.
    #[must_use]
    pub fn effective_config(&self, settings: &BTreeMap<String, Value>) -> EffectiveConfig {
        let mut config = self.defaults();
        let mut dropped = Vec::new();
        let mut invalid = Vec::new();
        for (key, value) in settings {
            match self.get(key) {
                Some(spec) if !spec.is_secret() => {
                    if spec.check_value(value).is_ok() {
                        config.insert(key.clone(), value.clone());
                    } else {
                        invalid.push(key.clone());
                    }
                }
                _ => dropped.push(key.clone()),
            }
        }
        let missing = self
            .config_entries()
            .filter(|spec| spec.required && !config.contains_key(&spec.key))
            .map(|spec| spec.key.clone())
            .collect();
        EffectiveConfig {
            config,
            dropped,
            invalid,
            missing,
        }
    }
}

/// Result of [`SettingsSchema::effective_config`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveConfig {
    /// Values delivered as `CONFIG`.
    pub config: BTreeMap<String, Value>,
    /// Document keys the schema does not declare.
    pub dropped: Vec<String>,
    /// Declared keys whose stored value does not fit the entry's kind.
    pub invalid: Vec<String>,
    /// Required keys with neither a usable value nor a default.
    pub missing: Vec<String>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::missing_panics_doc)]

    use super::*;

    fn spec(key: &str, kind: SettingKind) -> SettingSpec {
        SettingSpec {
            key: key.into(),
            label: String::new(),
            kind,
            values: Vec::new(),
            default: None,
            required: false,
            description: None,
        }
    }

    fn enum_spec(key: &str, ids: &[&str], default: Option<&str>) -> SettingSpec {
        SettingSpec {
            values: ids
                .iter()
                .map(|id| SettingValueSpec {
                    id: (*id).into(),
                    label: String::new(),
                })
                .collect(),
            default: default.map(|d| Value::String(d.into())),
            ..spec(key, SettingKind::Enum)
        }
    }

    #[test]
    fn toml_shape_parses() {
        let text = r#"
            [[settings]]
            key = "bitrate"
            label = "Bitrate"
            type = "enum"
            values = [{ id = "high", label = "High" }, { id = "normal" }]
            default = "high"

            [[settings]]
            key = "token"
            type = "secret"
        "#;
        let schema: SettingsSchema = toml::from_str(text).unwrap();
        schema.validate(true, true).unwrap();
        assert_eq!(schema.settings.len(), 2);
        assert_eq!(
            schema.defaults().get("bitrate"),
            Some(&Value::String("high".into()))
        );
        let options = schema.config_options();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].values[1].label, "normal");
    }

    #[test]
    fn validation_rules() {
        let bad_key = SettingsSchema::new(vec![spec("Bad", SettingKind::String)]);
        assert!(bad_key.validate(true, true).is_err());
        let dup = SettingsSchema::new(vec![
            spec("a", SettingKind::String),
            spec("a", SettingKind::Bool),
        ]);
        assert!(dup.validate(true, true).is_err());
        let empty_enum = SettingsSchema::new(vec![spec("a", SettingKind::Enum)]);
        assert!(empty_enum.validate(true, true).is_err());
        let bad_default = SettingsSchema::new(vec![enum_spec("a", &["x"], Some("y"))]);
        assert!(bad_default.validate(true, true).is_err());
        let secret_default = SettingsSchema::new(vec![SettingSpec {
            default: Some(Value::String("x".into())),
            ..spec("s", SettingKind::Secret)
        }]);
        assert!(secret_default.validate(true, true).is_err());
        let needs_secrets = SettingsSchema::new(vec![spec("s", SettingKind::Secret)]);
        assert!(needs_secrets.validate(true, false).is_err());
        let needs_vars = SettingsSchema::new(vec![spec("a", SettingKind::String)]);
        assert!(needs_vars.validate(false, true).is_err());
        assert!(needs_vars.validate(true, false).is_ok());
    }

    #[test]
    fn check_setting_and_effective_config() {
        let schema = SettingsSchema::new(vec![
            enum_spec("bitrate", &["high", "normal"], Some("high")),
            SettingSpec {
                required: true,
                ..spec("bucket", SettingKind::String)
            },
            spec("path_style", SettingKind::Bool),
            spec("token", SettingKind::Secret),
        ]);
        schema.validate(true, true).unwrap();
        assert!(schema
            .check_setting("bitrate", &Value::String("low".into()))
            .is_err());
        assert!(schema
            .check_setting("path_style", &Value::Bool(true))
            .is_ok());
        assert!(schema
            .check_setting("path_style", &Value::String("yes".into()))
            .is_err());
        assert!(schema
            .check_setting("token", &Value::String("x".into()))
            .is_err());
        assert!(schema.check_setting("nope", &Value::Bool(true)).is_err());
        assert!(schema.check_secret_key("token").is_ok());
        assert!(schema.check_secret_key("bucket").is_err());

        let mut doc = BTreeMap::new();
        doc.insert("path_style".to_string(), Value::Bool(true));
        doc.insert("stale".to_string(), Value::Bool(true));
        let effective = schema.effective_config(&doc);
        assert_eq!(
            effective.config.get("bitrate"),
            Some(&Value::String("high".into()))
        );
        assert_eq!(effective.config.get("path_style"), Some(&Value::Bool(true)));
        assert_eq!(effective.dropped, vec!["stale".to_string()]);
        assert!(effective.invalid.is_empty());
        assert_eq!(effective.missing, vec!["bucket".to_string()]);

        // A value that no longer fits its entry falls back to the default and
        // is reported; a required one with no default is missing.
        doc.insert("bitrate".to_string(), Value::String("low".into()));
        doc.insert("bucket".to_string(), Value::Bool(false));
        let effective = schema.effective_config(&doc);
        assert_eq!(
            effective.config.get("bitrate"),
            Some(&Value::String("high".into()))
        );
        assert!(!effective.config.contains_key("bucket"));
        assert_eq!(
            effective.invalid,
            vec!["bitrate".to_string(), "bucket".to_string()]
        );
        assert_eq!(effective.missing, vec!["bucket".to_string()]);
        let required_secret = SettingsSchema::new(vec![SettingSpec {
            required: true,
            ..spec("token", SettingKind::Secret)
        }]);
        assert_eq!(
            required_secret.required_secret_keys().collect::<Vec<_>>(),
            vec!["token"]
        );
    }
}
