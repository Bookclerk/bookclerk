//! Delivers the model plugin's models: pinned, downloaded by the host,
//! verified file by file.
//!
//! The model plugin (`embedder`) runs in a jail with no network, so the
//! host puts a model's files into the plugin's data directory. Which
//! models exist, and what each one's `model.json` must hash to, is a file
//! in the plugin package ([`MODEL_OFFERS_FILE`]); a host therefore only
//! ever installs the packs its own plugin build was released with.
//!
//! A pack is published as an OCI artifact
//! (`docs/internals/model-packs.md`), one blob per file, each addressed by
//! the file's SHA-256. The pin is the digest of `model.json`, and
//! `model.json` lists the size and digest of every other file, so the
//! host needs nothing else the registry says: it asks for `model.json` by
//! its digest, checks it, and asks for each listed file by the digest the
//! checked description gives. No tag and no registry manifest is read.
//!
//! The only reply the host parses is the registry's token reply
//! ([`parse_token_reply`]); the token goes into a request header, so only
//! a token made of header-safe characters is accepted.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use bookclerk_model_pack::{
    file_matches, PackManifest, Sha256Hex, MANIFEST_FILE, MAX_MANIFEST_BYTES,
};
use bookclerk_tls::ureq;
use serde::Deserialize;

use crate::error::{CatalogError, Result};
use crate::install::{download_resumable, Transfer, DOWNLOAD_TIMEOUT, MAX_REDIRECTS};

/// Name of the file in the model plugin's package that lists the models
/// it can be given and pins each one.
pub const MODEL_OFFERS_FILE: &str = "models.json";

/// Longest [`MODEL_OFFERS_FILE`] read, in bytes.
pub const MAX_MODEL_OFFERS_BYTES: usize = 64 * 1024;

/// Most models one plugin package may offer.
pub const MAX_MODEL_OFFERS: usize = 32;

/// Longest registry token reply read, in bytes.
pub const MAX_TOKEN_REPLY_BYTES: usize = 64 * 1024;

/// Longest bearer token accepted, in bytes.
pub const MAX_TOKEN_BYTES: usize = 8192;

/// Largest model accepted, all files together, in bytes.
pub const MAX_MODEL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Longest model id, label or registry part, in bytes.
const MAX_TEXT_BYTES: usize = 200;

/// Prefix of the directory a model is downloaded into before it replaces
/// the installed one. It starts with a dot, so it is never a model id and
/// the plugin never lists it.
const INCOMING_PREFIX: &str = ".incoming-";

/// Prefix of the directory an installed model is moved to while its
/// replacement takes its place.
const OUTGOING_PREFIX: &str = ".outgoing-";

/// Whether `id` can name a model: 1 to 64 lowercase letters, digits, `.`,
/// `_` and `-`, starting with a letter or digit (the rule `model.json`
/// keeps for its own `model_id`).
fn is_model_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let plain = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    (1..=64).contains(&bytes.len())
        && bytes.first().is_some_and(plain)
        && bytes
            .iter()
            .all(|b| plain(b) || matches!(b, b'.' | b'_' | b'-'))
}

/// The pin of a model: what its `model.json` must be. That file lists the
/// size and SHA-256 of every other file, so this one digest fixes the
/// whole model.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptionPin {
    /// Length of `model.json` in bytes.
    pub bytes: u64,
    /// SHA-256 of `model.json`.
    pub sha256: Sha256Hex,
}

impl DescriptionPin {
    /// The pin that `content` satisfies.
    #[must_use]
    pub fn of(content: &[u8]) -> Self {
        Self {
            bytes: u64::try_from(content.len()).unwrap_or(u64::MAX),
            sha256: Sha256Hex::of(content),
        }
    }

    /// Whether `content` is the pinned description.
    fn admits(&self, content: &[u8]) -> bool {
        u64::try_from(content.len()).is_ok_and(|len| len == self.bytes)
            && Sha256Hex::of(content) == self.sha256
    }
}

/// One file to download: its name in the model directory and what it
/// must be.
#[derive(Clone, Copy)]
struct Wanted<'a> {
    /// Name inside the model directory.
    name: &'a str,
    /// Length in bytes.
    bytes: u64,
    /// SHA-256 of the content, which is also its blob name.
    sha256: &'a Sha256Hex,
}

/// One model the plugin can be given.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOffer {
    /// Name shown in the picker.
    pub label: String,
    /// One sentence for the picker on what the model is good for.
    pub summary: String,
    /// The pin: size and SHA-256 of the model's `model.json`.
    pub model_json: DescriptionPin,
    /// Bytes downloaded and kept on disk, all files together.
    pub download_bytes: u64,
    /// Memory the plugin needs while the model is loaded, in bytes.
    pub memory_bytes: u64,
}

/// The models a plugin package offers and where they are published.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOffers {
    /// Registry host and repository prefix the packs are published under
    /// (`ghcr.io/bookclerk/models`); a model's repository is this plus
    /// `/<model id>`.
    registry: String,
    /// The model a library uses until it chooses another.
    default: String,
    /// The offered models by model id.
    models: BTreeMap<String, ModelOffer>,
}

impl ModelOffers {
    /// Reads and checks a [`MODEL_OFFERS_FILE`].
    ///
    /// # Arguments
    ///
    /// * `bytes` - The file's content.
    ///
    /// # Errors
    ///
    /// Returns an error when the file is too large, is not the expected
    /// JSON, names a registry or a model id that is not well formed, pins
    /// a description over the size limit, or names a default it does
    /// not offer.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_MODEL_OFFERS_BYTES {
            return Err(CatalogError::message("the model list is too large"));
        }
        let offers: Self = serde_json::from_slice(bytes)
            .map_err(|err| CatalogError::message(format!("the model list is not valid: {err}")))?;
        offers.check()?;
        Ok(offers)
    }

    /// Reads the model list of an installed plugin package.
    ///
    /// # Arguments
    ///
    /// * `install_dir` - The plugin's install directory.
    ///
    /// # Returns
    ///
    /// `None` when the package carries no model list.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or
    /// [`ModelOffers::parse`] refuses it.
    pub fn read(install_dir: &Path) -> Result<Option<Self>> {
        let path = install_dir.join(MODEL_OFFERS_FILE);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let mut bytes = Vec::new();
        let cap = u64::try_from(MAX_MODEL_OFFERS_BYTES)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        file.take(cap).read_to_end(&mut bytes)?;
        Self::parse(&bytes).map(Some)
    }

    /// Checks every rule a model list must keep.
    fn check(&self) -> Result<()> {
        let invalid = |what: &str| CatalogError::message(format!("the model list {what}"));
        let (host, repository) = self
            .registry
            .split_once('/')
            .ok_or_else(|| invalid("names no repository"))?;
        let host_ok = !host.is_empty()
            && host.len() <= MAX_TEXT_BYTES
            && !host.starts_with(['.', '-'])
            && host
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'));
        let repository_ok = repository.len() <= MAX_TEXT_BYTES
            && repository.split('/').all(|part| {
                part.bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                    && part.bytes().all(|b| {
                        b.is_ascii_lowercase()
                            || b.is_ascii_digit()
                            || matches!(b, b'.' | b'_' | b'-')
                    })
            });
        if !host_ok || !repository_ok {
            return Err(invalid("names a registry that is not well formed"));
        }
        if self.models.is_empty() || self.models.len() > MAX_MODEL_OFFERS {
            return Err(invalid("offers no model or too many"));
        }
        for (id, offer) in &self.models {
            if !is_model_id(id) {
                return Err(invalid("has a model id that is not well formed"));
            }
            let text_ok = |text: &str| {
                !text.is_empty()
                    && text.len() <= MAX_TEXT_BYTES
                    && !text.chars().any(char::is_control)
            };
            if !text_ok(&offer.label) || !text_ok(&offer.summary) {
                return Err(invalid("has a label or summary that is empty or too long"));
            }
            let description_fits =
                usize::try_from(offer.model_json.bytes).is_ok_and(|len| len <= MAX_MANIFEST_BYTES);
            if !description_fits {
                return Err(invalid("pins a model description over the size limit"));
            }
            if offer.download_bytes > MAX_MODEL_BYTES {
                return Err(invalid("offers a model over the size limit"));
            }
        }
        if !self.models.contains_key(&self.default) {
            return Err(invalid("names a default it does not offer"));
        }
        Ok(())
    }

    /// Id of the model a library uses until it chooses another.
    #[must_use]
    pub fn default_model(&self) -> &str {
        &self.default
    }

    /// The offered models, ascending by model id.
    pub fn models(&self) -> impl Iterator<Item = (&str, &ModelOffer)> {
        self.models.iter().map(|(id, offer)| (id.as_str(), offer))
    }

    /// One offered model.
    ///
    /// # Arguments
    ///
    /// * `model_id` - The model id.
    #[must_use]
    pub fn get(&self, model_id: &str) -> Option<&ModelOffer> {
        self.models.get(model_id)
    }

    /// The registry the packs are published in, reached over HTTPS.
    #[must_use]
    pub fn registry(&self) -> ModelRegistry {
        let (host, repository) = self
            .registry
            .split_once('/')
            .unwrap_or((self.registry.as_str(), ""));
        ModelRegistry {
            base: format!("https://{host}"),
            service: host.to_owned(),
            repository: repository.to_owned(),
        }
    }
}

/// Where model packs are downloaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRegistry {
    /// Scheme and authority every request starts with (`https://ghcr.io`).
    base: String,
    /// The `service` the token endpoint is asked for (the registry host).
    service: String,
    /// Repository prefix; a model's repository is this plus `/<model id>`.
    repository: String,
}

impl ModelRegistry {
    /// The same registry reached at another address: a mirror, or a test
    /// server on loopback.
    ///
    /// # Arguments
    ///
    /// * `base` - Scheme and authority, `https://…` or loopback `http://…`,
    ///   with no path.
    ///
    /// # Errors
    ///
    /// Returns an error for any other URL.
    pub fn at(&self, base: &str) -> Result<Self> {
        let url = url::Url::parse(base)?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost"));
        let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback);
        let bare = url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none();
        if !scheme_ok || !bare {
            return Err(CatalogError::message(
                "a model registry address is https:// (or loopback http://) with no path",
            ));
        }
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            service: self.service.clone(),
            repository: self.repository.clone(),
        })
    }

    /// URL that hands out a pull token for one model's repository.
    fn token_url(&self, model_id: &str) -> String {
        format!(
            "{}/token?service={}&scope=repository:{}/{}:pull",
            self.base, self.service, self.repository, model_id
        )
    }

    /// URL of the blob with a given SHA-256 in a model's repository.
    fn blob_url(&self, model_id: &str, sha256: &Sha256Hex) -> String {
        format!(
            "{}/v2/{}/{}/blobs/sha256:{}",
            self.base,
            self.repository,
            model_id,
            sha256.as_str()
        )
    }
}

/// A registry pull token, safe to put in a request header.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(String);

impl BearerToken {
    /// The token as it goes after `Bearer ` in an `Authorization` header.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BearerToken(…)")
    }
}

/// The one field read from a token reply.
#[derive(Deserialize)]
struct TokenReply {
    /// The bearer token.
    token: String,
}

/// Reads the bearer token from a registry's token reply.
///
/// The reply comes from the network and the token goes into a request
/// header, so only a token of 1 to [`MAX_TOKEN_BYTES`] characters from
/// `A-Z a-z 0-9 . _ ~ + / = -` (what RFC 6750 allows) is accepted.
///
/// # Arguments
///
/// * `body` - The reply body.
///
/// # Errors
///
/// Returns an error when the body is too large, is not a JSON object with
/// a `token` string, or the token holds any other character.
pub fn parse_token_reply(body: &[u8]) -> Result<BearerToken> {
    if body.len() > MAX_TOKEN_REPLY_BYTES {
        return Err(CatalogError::message(
            "the registry's token reply is too large",
        ));
    }
    let reply: TokenReply = serde_json::from_slice(body)
        .map_err(|_| CatalogError::message("the registry's token reply holds no token"))?;
    let usable = (1..=MAX_TOKEN_BYTES).contains(&reply.token.len())
        && reply.token.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
        });
    if usable {
        Ok(BearerToken(reply.token))
    } else {
        Err(CatalogError::message(
            "the registry's token reply holds no usable token",
        ))
    }
}

/// Whether a model directory holds the offered model: its `model.json` is
/// the pinned one and every file it lists is there with the listed size.
///
/// File contents are not hashed here (the plugin checks every file's
/// SHA-256 before it loads a model), so this is cheap enough to run at
/// every start.
///
/// # Arguments
///
/// * `offer` - The offered model.
/// * `model_dir` - `<models directory>/<model id>`.
#[must_use]
pub fn model_installed(offer: &ModelOffer, model_dir: &Path) -> bool {
    let Ok(manifest) = read_pinned_description(offer, &model_dir.join(MANIFEST_FILE)) else {
        return false;
    };
    let complete = manifest.files().all(|file| {
        fs::metadata(model_dir.join(file.file.as_str()))
            .is_ok_and(|meta| meta.is_file() && meta.len() == file.bytes)
    });
    complete
}

/// Reads a `model.json` and checks it is the pinned one.
fn read_pinned_description(offer: &ModelOffer, path: &Path) -> Result<PackManifest> {
    let mut bytes = Vec::new();
    let cap = offer.model_json.bytes.saturating_add(1);
    fs::File::open(path)?.take(cap).read_to_end(&mut bytes)?;
    if !offer.model_json.admits(&bytes) {
        return Err(CatalogError::message(
            "the model description is not the pinned one",
        ));
    }
    PackManifest::parse(&bytes).map_err(|err| CatalogError::message(err.to_string()))
}

/// Asks the registry for a pull token for one model's repository.
fn pull_token(registry: &ModelRegistry, model_id: &str) -> Result<BearerToken> {
    let mut response = ureq::get(registry.token_url(model_id))
        .header(
            "User-Agent",
            concat!("bookclerk/", env!("CARGO_PKG_VERSION"), " (model-download)"),
        )
        .config()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .max_redirects(MAX_REDIRECTS)
        .build()
        .call()
        .map_err(|err| CatalogError::message(format!("the model registry gave no token: {err}")))?;
    let mut body = Vec::new();
    let cap = u64::try_from(MAX_TOKEN_REPLY_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    response
        .body_mut()
        .as_reader()
        .take(cap)
        .read_to_end(&mut body)?;
    parse_token_reply(&body)
}

/// Downloads a model into the plugin's models directory and verifies it.
///
/// The files are downloaded into a directory beside the model's own and
/// take its place only when every one has the size and SHA-256 the pinned
/// `model.json` lists, so the plugin never sees a partial model. A file a
/// stopped download already completed is kept and not downloaded again.
/// This call blocks until the model is installed or the download fails.
///
/// # Arguments
///
/// * `registry` - Where the packs are published
///   ([`ModelOffers::registry`]).
/// * `model_id` - The model, one of the offered ids.
/// * `offer` - Its offer, with the pin.
/// * `models_dir` - The plugin's models directory; created when missing.
/// * `progress` - Called with the bytes on disk so far and the model's
///   total, as files arrive.
///
/// # Errors
///
/// Returns an error when the id is not well formed, the registry refuses
/// or keeps failing, a file is not the one listed, the description is not
/// the pinned one or names another model, or a file cannot be written.
pub fn fetch_model(
    registry: &ModelRegistry,
    model_id: &str,
    offer: &ModelOffer,
    models_dir: &Path,
    progress: &(dyn Fn(u64, u64) + Sync),
) -> Result<()> {
    if !is_model_id(model_id) {
        return Err(CatalogError::message("the model id is not well formed"));
    }
    let incoming = models_dir.join(format!("{INCOMING_PREFIX}{model_id}"));
    fs::create_dir_all(&incoming)?;
    let token = pull_token(registry, model_id)?;

    let description_path = incoming.join(MANIFEST_FILE);
    let manifest = match read_pinned_description(offer, &description_path) {
        Ok(manifest) => manifest,
        Err(_) => {
            let description = Wanted {
                name: MANIFEST_FILE,
                bytes: offer.model_json.bytes,
                sha256: &offer.model_json.sha256,
            };
            fetch_file(registry, model_id, description, &incoming, &token, &|_| {})?;
            read_pinned_description(offer, &description_path)?
        }
    };
    if manifest.model_id != model_id {
        return Err(CatalogError::message(
            "the pinned model description is for another model",
        ));
    }
    let total = manifest.files().fold(offer.model_json.bytes, |sum, file| {
        sum.saturating_add(file.bytes)
    });
    if total > MAX_MODEL_BYTES {
        return Err(CatalogError::message("the model is over the size limit"));
    }

    let mut done = offer.model_json.bytes;
    progress(done, total);
    for file in manifest.files() {
        let path = incoming.join(file.file.as_str());
        if file.verify_file(&path).is_err() {
            let before = done;
            let wanted = Wanted {
                name: file.file.as_str(),
                bytes: file.bytes,
                sha256: &file.sha256,
            };
            fetch_file(registry, model_id, wanted, &incoming, &token, &|offset| {
                progress(before.saturating_add(offset), total);
            })?;
        }
        done = done.saturating_add(file.bytes);
        progress(done, total);
    }
    remove_unlisted(&incoming, &manifest)?;
    put_in_place(&incoming, models_dir, model_id)
}

/// Downloads one file of a model as a blob and checks it.
fn fetch_file(
    registry: &ModelRegistry,
    model_id: &str,
    file: Wanted<'_>,
    dir: &Path,
    token: &BearerToken,
    progress: &(dyn Fn(u64) + Sync),
) -> Result<()> {
    let path = dir.join(file.name);
    let transfer = Transfer {
        bearer: Some(token.as_str()),
        max_bytes: file.bytes,
        purpose: "model-download",
        progress: Some(progress),
    };
    download_resumable(&registry.blob_url(model_id, file.sha256), &path, &transfer)?;
    if !file_matches(&path, file.bytes, file.sha256) {
        // Never leave bytes that are not the listed file where a later
        // attempt could take them for a finished download.
        let _ = fs::remove_file(&path);
        return Err(CatalogError::message(format!(
            "the downloaded model file `{}` was refused: it is not the file listed",
            file.name
        )));
    }
    Ok(())
}

/// Removes everything in a download directory that the description does
/// not list, so an installed model holds exactly its files.
fn remove_unlisted(incoming: &Path, manifest: &PackManifest) -> Result<()> {
    for entry in fs::read_dir(incoming)? {
        let entry = entry?;
        let name = entry.file_name();
        let listed = name.to_str().is_some_and(|name| {
            name == MANIFEST_FILE || manifest.files().any(|file| file.file.as_str() == name)
        });
        if !listed {
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
    }
    Ok(())
}

/// Moves a complete download to the model's directory, replacing what was
/// there.
fn put_in_place(incoming: &Path, models_dir: &Path, model_id: &str) -> Result<()> {
    let installed = models_dir.join(model_id);
    let outgoing: PathBuf = models_dir.join(format!("{OUTGOING_PREFIX}{model_id}"));
    if outgoing.exists() {
        fs::remove_dir_all(&outgoing)?;
    }
    let replaced = match fs::rename(&installed, &outgoing) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };
    if let Err(err) = fs::rename(incoming, &installed) {
        if replaced {
            // Put the old model back; a failed swap must not lose it.
            let _ = fs::rename(&outgoing, &installed);
        }
        return Err(err.into());
    }
    if replaced {
        fs::remove_dir_all(&outgoing)?;
    }
    Ok(())
}

/// Removes model directories and stopped downloads the plugin should no
/// longer hold.
///
/// # Arguments
///
/// * `models_dir` - The plugin's models directory.
/// * `keep` - Model ids to keep, installed or still downloading.
///
/// # Errors
///
/// Returns an error when the directory cannot be read or an entry cannot
/// be removed. A models directory that does not exist is not an error.
pub fn remove_other_models(models_dir: &Path, keep: &[&str]) -> Result<()> {
    let entries = match fs::read_dir(models_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let model = name
            .strip_prefix(INCOMING_PREFIX)
            .or_else(|| name.strip_prefix(OUTGOING_PREFIX))
            .unwrap_or(name);
        let ours = is_model_id(model) && entry.file_type()?.is_dir();
        if ours && !keep.contains(&model) {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    use bookclerk_model_pack::Sha256Hex;

    use super::*;

    /// A tiny model: its files by name, `model.json` among them.
    struct Pack {
        /// File content by name.
        files: BTreeMap<String, Vec<u8>>,
    }

    impl Pack {
        /// A model with three small files and the description that lists
        /// them.
        fn new(model_id: &str) -> Self {
            let mut files = BTreeMap::new();
            files.insert("core.onnx".to_owned(), b"graph bytes".repeat(300));
            files.insert("tokenizer.spm.fb".to_owned(), b"pieces".to_vec());
            files.insert(
                "base.rows".to_owned(),
                (0..=255u8).cycle().take(9000).collect(),
            );
            let entry = |name: &str| {
                let content = &files[name];
                serde_json::json!({
                    "file": name,
                    "bytes": content.len(),
                    "sha256": Sha256Hex::of(content).as_str(),
                })
            };
            let description = serde_json::json!({
                "format": 1,
                "model_id": model_id,
                "dimensions": 4,
                "max_tokens": 16,
                "graph": {
                    "model": entry("core.onnx"),
                    "ids_input": "input_ids",
                    "mask_input": "attention_mask",
                    "output": "sentence_embedding",
                    "pooling": "none",
                },
                "tokenizer": { "model": entry("tokenizer.spm.fb") },
                "tables": [{ "input": "rows", "element": "u8", "width": 4 }],
                "base_rows": entry("base.rows"),
                "languages": {},
            });
            let mut bytes = serde_json::to_vec_pretty(&description).expect("json");
            bytes.push(b'\n');
            PackManifest::parse(&bytes).expect("a valid description");
            files.insert(MANIFEST_FILE.to_owned(), bytes);
            Self { files }
        }

        /// The offer that pins this model.
        fn offer(&self) -> ModelOffer {
            let description = &self.files[MANIFEST_FILE];
            ModelOffer {
                label: "Tiny".into(),
                summary: "A model for tests.".into(),
                model_json: DescriptionPin::of(description),
                download_bytes: self.files.values().map(|f| f.len() as u64).sum(),
                memory_bytes: 1,
            }
        }

        /// Content by `sha256:<hex>` blob name.
        fn blobs(&self) -> BTreeMap<String, Vec<u8>> {
            self.files
                .values()
                .map(|content| (Sha256Hex::of(content).as_str().to_owned(), content.clone()))
                .collect()
        }
    }

    /// What the fake registry was asked: request line and whether the
    /// request carried the token.
    type Seen = Arc<Mutex<Vec<(String, bool)>>>;

    /// A registry on loopback that hands out the token `t0ken` and serves
    /// `blobs` to requests that carry it. `cut` truncates the first answer
    /// for the blob with that digest, to exercise a resumed download.
    fn registry(blobs: BTreeMap<String, Vec<u8>>, cut: Option<String>) -> (ModelRegistry, Seen) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            let mut cut = cut;
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { return };
                let mut request = Vec::new();
                let mut buf = [0u8; 2048];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&request).into_owned();
                let line = text.lines().next().unwrap_or_default().to_owned();
                let header = |name: &str| {
                    text.lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix(name).map(str::to_owned))
                };
                let authorized =
                    header("authorization:").is_some_and(|v| v.trim() == "bearer t0ken");
                log.lock().expect("log").push((line.clone(), authorized));
                let path = line.split(' ').nth(1).unwrap_or_default();
                let answer = if path.starts_with("/token?") {
                    let body = br#"{"token":"t0ken","expires_in":300}"#;
                    let mut out = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    out.extend_from_slice(body);
                    out
                } else if let Some(body) = path
                    .rsplit_once("/blobs/sha256:")
                    .filter(|_| authorized)
                    .and_then(|(_, digest)| blobs.get(digest).map(|body| (digest, body)))
                    .map(|(digest, body)| {
                        let from = header("range:").and_then(|v| {
                            v.trim()
                                .strip_prefix("bytes=")?
                                .strip_suffix('-')?
                                .parse::<usize>()
                                .ok()
                        });
                        (digest.to_owned(), body, from)
                    })
                {
                    let (digest, body, from) = body;
                    match from {
                        Some(from) => {
                            let mut out = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                                body.len() - from,
                                from,
                                body.len() - 1,
                                body.len()
                            )
                            .into_bytes();
                            out.extend_from_slice(&body[from..]);
                            out
                        }
                        None => {
                            let mut out = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"{digest}\"\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            if cut.as_deref() == Some(digest.as_str()) {
                                cut = None;
                                out.extend_from_slice(&body[..body.len() / 2]);
                            } else {
                                out.extend_from_slice(body);
                            }
                            out
                        }
                    }
                } else {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                };
                let _ = socket.write_all(&answer);
                let _ = socket.flush();
            }
        });
        let offers = ModelOffers::parse(&offers_json(&Pack::new("tiny"))).expect("offers");
        (offers.registry().at(&base).expect("loopback"), seen)
    }

    /// A model list that offers `pack` as `tiny`, the default.
    fn offers_json(pack: &Pack) -> Vec<u8> {
        let offer = pack.offer();
        serde_json::to_vec(&serde_json::json!({
            "registry": "registry.example/acme/models",
            "default": "tiny",
            "models": { "tiny": {
                "label": offer.label,
                "summary": offer.summary,
                "model_json": {
                    "bytes": offer.model_json.bytes,
                    "sha256": offer.model_json.sha256.as_str(),
                },
                "download_bytes": offer.download_bytes,
                "memory_bytes": offer.memory_bytes,
            } },
        }))
        .expect("json")
    }

    #[test]
    fn a_model_is_downloaded_verified_and_put_in_place() {
        let pack = Pack::new("tiny");
        let (registry, seen) = registry(pack.blobs(), None);
        let dir = tempfile::tempdir().expect("dir");
        let models = dir.path().join("models");
        let offer = pack.offer();
        assert!(!model_installed(&offer, &models.join("tiny")));

        let reported = Mutex::new(Vec::new());
        fetch_model(&registry, "tiny", &offer, &models, &|done, total| {
            reported.lock().expect("log").push((done, total));
        })
        .expect("download");

        for (name, content) in &pack.files {
            assert_eq!(
                &fs::read(models.join("tiny").join(name)).expect("file"),
                content
            );
        }
        assert!(model_installed(&offer, &models.join("tiny")));
        assert!(!models.join(".incoming-tiny").exists());
        let reported = reported.into_inner().expect("log");
        assert_eq!(
            reported.last(),
            Some(&(offer.download_bytes, offer.download_bytes))
        );
        assert!(reported.windows(2).all(|pair| pair[0].0 <= pair[1].0));

        let seen = seen.lock().expect("log").clone();
        assert!(seen[0]
            .0
            .contains("/token?service=registry.example&scope=repository:acme/models/tiny:pull"));
        assert!(!seen[0].1, "the token request carries no credentials");
        assert!(seen[1..].iter().all(|(line, authorized)| {
            line.contains("/v2/acme/models/tiny/blobs/sha256:") && *authorized
        }));
        assert_eq!(seen.len(), 1 + pack.files.len());
    }

    #[test]
    fn a_dropped_file_resumes_and_a_stopped_download_keeps_finished_files() {
        let pack = Pack::new("tiny");
        let rows = Sha256Hex::of(&pack.files["base.rows"]).as_str().to_owned();
        let (registry, seen) = registry(pack.blobs(), Some(rows.clone()));
        let dir = tempfile::tempdir().expect("dir");
        let models = dir.path().join("models");
        // An earlier attempt finished the graph and left a broken tokenizer.
        let incoming = models.join(".incoming-tiny");
        fs::create_dir_all(&incoming).expect("dir");
        fs::write(incoming.join("core.onnx"), &pack.files["core.onnx"]).expect("write");
        fs::write(incoming.join("tokenizer.spm.fb"), b"half").expect("write");
        fs::write(incoming.join("stray"), b"left behind").expect("write");

        fetch_model(&registry, "tiny", &pack.offer(), &models, &|_, _| {}).expect("download");

        assert!(model_installed(&pack.offer(), &models.join("tiny")));
        assert!(!models.join("tiny").join("stray").exists());
        let seen = seen.lock().expect("log").clone();
        let graph = Sha256Hex::of(&pack.files["core.onnx"]).as_str().to_owned();
        assert!(!seen.iter().any(|(line, _)| line.contains(&graph)));
        assert_eq!(
            seen.iter().filter(|(line, _)| line.contains(&rows)).count(),
            2
        );
    }

    #[test]
    fn a_file_that_is_not_the_listed_one_is_refused() {
        let pack = Pack::new("tiny");
        let mut blobs = pack.blobs();
        let rows = Sha256Hex::of(&pack.files["base.rows"]).as_str().to_owned();
        // Same length, other bytes, under the listed digest.
        blobs.insert(rows, vec![7u8; pack.files["base.rows"].len()]);
        let (registry, _) = registry(blobs, None);
        let dir = tempfile::tempdir().expect("dir");
        let models = dir.path().join("models");

        let err = fetch_model(&registry, "tiny", &pack.offer(), &models, &|_, _| {})
            .expect_err("refused");
        assert!(err.to_string().contains("refused"), "{err}");
        assert!(!models.join("tiny").exists());
        assert!(!models.join(".incoming-tiny").join("base.rows").exists());
    }

    #[test]
    fn a_description_that_is_not_the_pinned_one_is_refused() {
        let pack = Pack::new("tiny");
        let other = Pack::new("other");
        let (registry, _) = registry(other.blobs(), None);
        let dir = tempfile::tempdir().expect("dir");
        let models = dir.path().join("models");
        // The registry does not hold the pinned description at all.
        assert!(fetch_model(&registry, "tiny", &pack.offer(), &models, &|_, _| {}).is_err());
        // A pin for a description of another model is refused after the
        // description itself checks out.
        let (registry, _) = self::registry(other.blobs(), None);
        let err = fetch_model(&registry, "tiny", &other.offer(), &models, &|_, _| {})
            .expect_err("another model");
        assert!(err.to_string().contains("another model"), "{err}");
        assert!(!models.join("tiny").exists());
    }

    #[test]
    fn a_new_model_replaces_the_installed_one_and_others_are_removed() {
        let pack = Pack::new("tiny");
        let (registry, _) = registry(pack.blobs(), None);
        let dir = tempfile::tempdir().expect("dir");
        let models = dir.path().join("models");
        fs::create_dir_all(models.join("tiny")).expect("dir");
        fs::write(models.join("tiny").join("old"), b"old").expect("write");
        fs::create_dir_all(models.join("retired")).expect("dir");
        fs::create_dir_all(models.join(".incoming-retired")).expect("dir");
        fs::create_dir_all(models.join("Not A Model")).expect("dir");

        fetch_model(&registry, "tiny", &pack.offer(), &models, &|_, _| {}).expect("download");
        assert!(!models.join("tiny").join("old").exists());
        assert!(!models.join(".outgoing-tiny").exists());

        remove_other_models(&models, &["tiny"]).expect("sweep");
        assert!(models.join("tiny").is_dir());
        assert!(!models.join("retired").exists());
        assert!(!models.join(".incoming-retired").exists());
        assert!(
            models.join("Not A Model").is_dir(),
            "only model directories are removed"
        );
        remove_other_models(&dir.path().join("absent"), &[]).expect("nothing to do");
    }

    #[test]
    fn model_lists_are_checked() {
        let pack = Pack::new("tiny");
        let good: serde_json::Value = serde_json::from_slice(&offers_json(&pack)).expect("json");
        let offers = ModelOffers::parse(&offers_json(&pack)).expect("offers");
        assert_eq!(offers.default_model(), "tiny");
        assert_eq!(
            offers.models().map(|(id, _)| id).collect::<Vec<_>>(),
            ["tiny"]
        );
        assert_eq!(offers.get("tiny"), Some(&pack.offer()));
        assert_eq!(
            offers
                .registry()
                .blob_url("tiny", &pack.offer().model_json.sha256),
            format!(
                "https://registry.example/v2/acme/models/tiny/blobs/sha256:{}",
                pack.offer().model_json.sha256.as_str()
            )
        );

        let broken = |change: &dyn Fn(&mut serde_json::Value)| {
            let mut value = good.clone();
            change(&mut value);
            ModelOffers::parse(&serde_json::to_vec(&value).expect("json")).is_err()
        };
        assert!(broken(&|v| v["registry"] = "registry.example".into()));
        assert!(broken(&|v| v["registry"] = "Registry.example/a".into()));
        assert!(broken(&|v| v["registry"] = "registry.example/a/../b".into()));
        assert!(broken(&|v| v["registry"] = "user@registry.example/a".into()));
        assert!(broken(&|v| v["default"] = "absent".into()));
        assert!(broken(&|v| v["models"] = serde_json::json!({})));
        assert!(broken(
            &|v| v["models"]["Tiny"] = v["models"]["tiny"].clone()
        ));
        assert!(broken(&|v| v["models"]["tiny"]["label"] = "".into()));
        assert!(broken(
            &|v| v["models"]["tiny"]["model_json"]["sha256"] = "ABC".into()
        ));
        assert!(broken(
            &|v| v["models"]["tiny"]["model_json"]["bytes"] = (1u64 << 40).into()
        ));
        assert!(broken(
            &|v| v["models"]["tiny"]["download_bytes"] = u64::MAX.into()
        ));
        assert!(broken(&|v| v["models"]["tiny"]["extra"] = 1.into()));
        assert!(ModelOffers::parse(&vec![b' '; MAX_MODEL_OFFERS_BYTES + 1]).is_err());
    }

    #[test]
    fn registry_addresses_are_https_or_loopback() {
        let registry = ModelOffers::parse(&offers_json(&Pack::new("tiny")))
            .expect("offers")
            .registry();
        for allowed in [
            "https://mirror.example",
            "http://127.0.0.1:5000",
            "http://localhost:5000/",
        ] {
            assert!(registry.at(allowed).is_ok(), "{allowed}");
        }
        for refused in [
            "http://mirror.example",
            "https://mirror.example/path",
            "https://user:pw@mirror.example",
            "https://mirror.example?x=1",
            "file:///tmp",
            "mirror.example",
        ] {
            assert!(registry.at(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn token_replies_give_only_header_safe_tokens() {
        let token =
            parse_token_reply(br#"{"token":"abc.DEF-123_~+/=","expires_in":300}"#).expect("token");
        assert_eq!(token.as_str(), "abc.DEF-123_~+/=");
        assert_eq!(format!("{token:?}"), "BearerToken(…)");
        for refused in [
            &b""[..],
            b"not json",
            b"[]",
            b"{}",
            br#"{"token":null}"#,
            br#"{"token":""}"#,
            br#"{"token":"two words"}"#,
            br#"{"token":"line\r\nInjected: 1"}"#,
            br#"{"token":"quote\""}"#,
            "{\"token\":\"caf\u{e9}\"}".as_bytes(),
        ] {
            assert!(parse_token_reply(refused).is_err(), "{refused:?}");
        }
        let long = format!(r#"{{"token":"{}"}}"#, "a".repeat(MAX_TOKEN_BYTES + 1));
        assert!(parse_token_reply(long.as_bytes()).is_err());
        let large = format!(
            r#"{{"token":"a","pad":"{}"}}"#,
            "p".repeat(MAX_TOKEN_REPLY_BYTES)
        );
        assert!(parse_token_reply(large.as_bytes()).is_err());
    }
}
