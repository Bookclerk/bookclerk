//! [`ContentSource`] adapter over an external plugin process.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_library::LibraryStore;
use bookclerk_source::{
    ContentSource, FetchOptions, LoginOptions, PlainAudioPart, PlainFetch, PortalAuthMode,
    ScanOptions, ScanSummary, SourceAccount, SourceBrand, SourceFetch, SourceRegistry,
};
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::protocol::{
    methods, FetchTitleParams, LoginParams, ScanParams, ScanSummaryDto, SourceAccountDto,
    SourceFetchDto,
};
use crate::rpc::PluginClient;
use crate::Result;

/// External content source backed by a discovered plugin binary.
pub struct ExternalSource {
    client: PluginClient,
    display_name: String,
    brand: SourceBrand,
    auth_mode: PortalAuthMode,
    suffixes: &'static [&'static str],
    aliases: &'static [&'static str],
    password_env: Option<&'static str>,
    sort_key: u32,
    library_db: PathBuf,
    /// `[sources.<id>]` table from main config (also sent on handshake).
    source_config: Value,
}

impl ExternalSource {
    /// Spawn and handshake a source plugin.
    pub async fn spawn(plugin: &DiscoveredPlugin, config: &Config) -> Result<Self> {
        let table = crate::settings_table(config, plugin);
        let config_json = toml_to_json(&toml::Value::Table(table));
        let client = PluginClient::spawn(
            &plugin.manifest.id,
            &plugin.command,
            &plugin.manifest.args,
            &plugin.root,
            config_json.clone(),
        )
        .await?;
        let hs = client.handshake().clone();
        let display_name = hs
            .display_name
            .clone()
            .or_else(|| plugin.manifest.name.clone())
            .unwrap_or_else(|| plugin.manifest.id.clone());
        let brand = brand_from_dto(hs.brand.as_ref(), &plugin.manifest.id, &display_name);
        let auth_mode = match hs.portal_auth_mode.as_deref() {
            Some("oauth") => PortalAuthMode::Oauth,
            _ => PortalAuthMode::Password,
        };
        let suffixes = leak_str_slice(&hs.auth_credential_suffixes, &[".auth"]);
        let aliases = leak_str_slice(&hs.aliases, &[]);
        let password_env = hs
            .password_env_var
            .as_deref()
            .map(|s| Box::leak(s.to_string().into_boxed_str()) as &'static str);
        Ok(Self {
            client,
            display_name,
            brand,
            auth_mode,
            suffixes,
            aliases,
            password_env,
            sort_key: hs.sort_key.unwrap_or(200),
            library_db: config.paths().library_db.clone(),
            source_config: config_json,
        })
    }
}

/// Discover and register external source plugins.
///
/// Duplicate `(kind, id)` claims among discovered manifests are a hard error
/// (from [`crate::discover_plugins`]). An external id that collides with an
/// already-registered source is also fatal.
pub async fn load_external_sources(config: &Config, registry: &mut SourceRegistry) -> Result<()> {
    for plugin in crate::discover_plugins(config)? {
        if plugin.manifest.kind != crate::PluginKind::Source {
            continue;
        }
        if !config.sources.is_enabled(&plugin.manifest.id) {
            continue;
        }
        if registry.get(&plugin.manifest.id).is_some() {
            return Err(crate::PluginError::message(format!(
                "external source plugin id `{}` conflicts with an already registered source ({})",
                plugin.manifest.id,
                plugin.root.join("plugin.toml").display()
            )));
        }
        match ExternalSource::spawn(&plugin, config).await {
            Ok(s) => {
                tracing::info!(id = %plugin.manifest.id, "loaded external source plugin");
                registry.register(Arc::new(s));
            }
            Err(err) => {
                tracing::warn!(id = %plugin.manifest.id, %err, "skipping external source plugin");
            }
        }
    }
    Ok(())
}

#[async_trait]
impl ContentSource for ExternalSource {
    fn id(&self) -> &str {
        self.client.plugin_id()
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn aliases(&self) -> &'static [&'static str] {
        self.aliases
    }

    fn portal_auth_mode(&self) -> PortalAuthMode {
        self.auth_mode
    }

    fn portal_brand(&self) -> SourceBrand {
        self.brand
    }

    fn auth_credential_suffixes(&self) -> &'static [&'static str] {
        self.suffixes
    }

    fn password_env_var(&self) -> Option<&'static str> {
        self.password_env
    }

    fn sort_key(&self) -> u32 {
        self.sort_key
    }

    async fn login(
        &self,
        files_dir: &Path,
        opts: LoginOptions,
    ) -> bookclerk_source::Result<SourceAccount> {
        let dto: SourceAccountDto = self
            .client
            .call(
                methods::LOGIN,
                serde_json::to_value(LoginParams {
                    files_dir: files_dir.display().to_string(),
                    marketplace: opts.marketplace,
                    label: opts.label,
                    email: opts.email,
                    password: opts.password,
                    force: opts.force,
                })
                .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await?;
        Ok(account_from_dto(dto))
    }

    async fn list_accounts(
        &self,
        files_dir: &Path,
    ) -> bookclerk_source::Result<Vec<SourceAccount>> {
        let list: Vec<SourceAccountDto> = self
            .client
            .call(
                methods::LIST_ACCOUNTS,
                serde_json::json!({ "files_dir": files_dir.display().to_string() }),
            )
            .await?;
        Ok(list.into_iter().map(account_from_dto).collect())
    }

    async fn scan(
        &self,
        files_dir: &Path,
        _library: &LibraryStore,
        opts: ScanOptions,
    ) -> bookclerk_source::Result<ScanSummary> {
        let dto: ScanSummaryDto = self
            .client
            .call(
                methods::SCAN,
                serde_json::to_value(ScanParams {
                    files_dir: files_dir.display().to_string(),
                    library_db: self.library_db.display().to_string(),
                    accounts: opts.accounts,
                    page_size: opts.page_size,
                    import_episodes: opts.import_episodes,
                    import_plus_titles: opts.import_plus_titles,
                })
                .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await?;
        Ok(ScanSummary {
            accounts: dto.accounts,
            books_upserted: dto.books_upserted,
            pages: dto.pages,
            skipped_disabled: dto.skipped_disabled,
        })
    }

    async fn fetch_title(
        &self,
        files_dir: &Path,
        account_id: &str,
        title_id: &str,
        opts: &FetchOptions,
    ) -> bookclerk_source::Result<SourceFetch> {
        let dto: SourceFetchDto = self
            .client
            .call(
                methods::FETCH_TITLE,
                serde_json::to_value(FetchTitleParams {
                    files_dir: files_dir.display().to_string(),
                    account_id: account_id.to_string(),
                    title_id: title_id.to_string(),
                    cache_dir: opts.cache_dir.display().to_string(),
                    source_config: self.source_config.clone(),
                })
                .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await?;
        match dto {
            SourceFetchDto::Plain {
                parts,
                m4b_path,
                cover_path,
                chapters,
            } => Ok(SourceFetch::Plain(PlainFetch {
                parts: parts
                    .into_iter()
                    .map(|p| PlainAudioPart {
                        path: PathBuf::from(p.path),
                        title: p.title,
                        duration_ms: p.duration_ms,
                    })
                    .collect(),
                m4b_path: m4b_path.map(PathBuf::from),
                cover_path: cover_path.map(PathBuf::from),
                chapters,
            })),
        }
    }
}

fn account_from_dto(dto: SourceAccountDto) -> SourceAccount {
    SourceAccount {
        account_id: dto.account_id,
        source: dto.source,
        marketplace: dto.marketplace,
        label: dto.label,
        scan_enabled: dto.scan_enabled,
    }
}

fn leak_str_slice(owned: &[String], fallback: &[&'static str]) -> &'static [&'static str] {
    if owned.is_empty() {
        return Box::leak(fallback.to_vec().into_boxed_slice());
    }
    let leaked: Vec<&'static str> = owned
        .iter()
        .map(|s| Box::leak(s.clone().into_boxed_str()) as &'static str)
        .collect();
    Box::leak(leaked.into_boxed_slice())
}

fn brand_from_dto(dto: Option<&crate::protocol::BrandDto>, id: &str, name: &str) -> SourceBrand {
    if let Some(b) = dto {
        SourceBrand {
            id: Box::leak(b.id.clone().into_boxed_str()),
            name: Box::leak(b.name.clone().into_boxed_str()),
            bg: Box::leak(b.bg.clone().into_boxed_str()),
            fg: Box::leak(b.fg.clone().into_boxed_str()),
            accent: Box::leak(b.accent.clone().into_boxed_str()),
            icon_url: Box::leak(b.icon_url.clone().into_boxed_str()),
        }
    } else {
        SourceBrand {
            id: Box::leak(id.to_string().into_boxed_str()),
            name: Box::leak(name.to_string().into_boxed_str()),
            bg: "#334155",
            fg: "#f8fafc",
            accent: "#64748b",
            icon_url: "",
        }
    }
}

fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}
