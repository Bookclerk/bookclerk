//! A model directory: what is in it, and the model loaded from it.
//!
//! [`inspect`] is cheap: it reads `model.json` and looks at file sizes, so
//! the plugin can list its models without loading one. [`load`] reads and
//! verifies every file against the SHA-256 the description lists, then
//! builds the tokenizer, the row packs and the graph.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use bookclerk_model_pack::{
    revision, LanguageTag, PackError, PackFile, PackManifest, Rows, MANIFEST_FILE,
    MAX_MANIFEST_BYTES,
};
use bookclerk_plugin_sdk::{AiModel, AiTask};
use ort::environment::Environment;

use crate::engine::{EngineError, Graph};
use crate::text::{Role, TextEncoder, TextError};

/// Why a model directory could not be listed, loaded or run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    /// A file could not be read.
    #[error("cannot read `{file}`: {reason}")]
    Io {
        /// File name inside the model directory.
        file: String,
        /// The operating system's reason.
        reason: String,
    },
    /// A file is not what the layout or the model description says.
    #[error(transparent)]
    Pack(#[from] PackError),
    /// The directory's name is not the model id its description gives.
    #[error("the directory is named `{directory}` but describes model `{model}`")]
    Name {
        /// The directory's name.
        directory: String,
        /// `model_id` of its `model.json`.
        model: String,
    },
    /// A file every text needs is missing or has another length.
    #[error("`{0}` is missing or incomplete")]
    Incomplete(String),
    /// The tokenizer could not be set up or failed on a text.
    #[error(transparent)]
    Text(#[from] TextError),
    /// ONNX Runtime could not load or run the graph.
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// The base rows lack a token every text needs.
    #[error("the base rows have no row for token {0}")]
    BaseRow(u32),
    /// The graph gave a number that is not finite.
    #[error("the model gave a number that is not finite")]
    NotFinite,
}

/// What [`inspect`] found in a model directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The model directory.
    dir: PathBuf,
    /// The bytes of `model.json`, kept for the revision.
    manifest_bytes: Vec<u8>,
    /// The checked description.
    manifest: PackManifest,
    /// Tags of the language packs present, ascending.
    languages: Vec<LanguageTag>,
    /// Revision of the model with exactly these language packs.
    revision: String,
}

/// Whether `file` is present in `dir` as a regular file of the listed length.
fn is_present(dir: &Path, file: &PackFile) -> bool {
    fs::metadata(dir.join(file.file.as_str()))
        .is_ok_and(|meta| meta.is_file() && meta.len() == file.bytes)
}

/// Reads at most `limit` bytes of `dir/name`.
fn read_capped(dir: &Path, name: &str, limit: u64) -> Result<Vec<u8>, ModelError> {
    let io = |err: std::io::Error| ModelError::Io {
        file: name.to_owned(),
        reason: err.to_string(),
    };
    let mut bytes = Vec::new();
    fs::File::open(dir.join(name))
        .map_err(io)?
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    Ok(bytes)
}

/// Reads a listed file whole and checks it against the description.
fn read_listed(dir: &Path, file: &PackFile) -> Result<Vec<u8>, ModelError> {
    // One byte past the listed length shows a longer file without reading it all.
    let bytes = read_capped(dir, file.file.as_str(), file.bytes.saturating_add(1))?;
    file.verify(&bytes)?;
    Ok(bytes)
}

/// Looks at a model directory without loading the model.
///
/// # Arguments
///
/// * `dir` - `models/<model id>` in the plugin's data directory.
///
/// # Returns
///
/// The description and the language packs present on disk.
///
/// # Errors
///
/// When `model.json` is missing or invalid, the directory is named
/// differently from the model, or the graph, the tokenizer or the base rows
/// are missing or have another length than listed.
pub fn inspect(dir: &Path) -> Result<Listing, ModelError> {
    let limit = u64::try_from(MAX_MANIFEST_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let manifest_bytes = read_capped(dir, MANIFEST_FILE, limit)?;
    let manifest = PackManifest::parse(&manifest_bytes)?;
    let directory = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if directory != manifest.model_id {
        return Err(ModelError::Name {
            directory,
            model: manifest.model_id,
        });
    }
    for file in [
        &manifest.graph.model,
        &manifest.tokenizer.model,
        &manifest.base_rows,
    ] {
        if !is_present(dir, file) {
            return Err(ModelError::Incomplete(file.file.as_str().to_owned()));
        }
    }
    let languages: Vec<LanguageTag> = manifest
        .languages
        .iter()
        .filter(|(_, file)| is_present(dir, file))
        .map(|(tag, _)| tag.clone())
        .collect();
    let revision = revision(&manifest_bytes, &languages);
    Ok(Listing {
        dir: dir.to_path_buf(),
        manifest_bytes,
        manifest,
        languages,
        revision,
    })
}

impl Listing {
    /// Revision of the model with the language packs present.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// The model as the plugin lists it to the host.
    #[must_use]
    pub fn info(&self) -> AiModel {
        AiModel {
            model_id: self.manifest.model_id.clone(),
            task: AiTask::TextEmbeddings,
            revision: self.revision.clone(),
            dimensions: Some(self.manifest.dimensions),
            max_tokens: Some(self.manifest.max_tokens),
            languages: self
                .languages
                .iter()
                .map(|tag| tag.as_str().to_owned())
                .collect(),
        }
    }
}

/// One text's vector and whether a pack was missing for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    /// The vector, scaled to length one.
    pub vector: Vec<f32>,
    /// True when a token of the text had no row in the loaded packs and
    /// was spelled with smaller pieces.
    pub fallback: bool,
}

/// A model in memory.
#[derive(Debug)]
pub struct LoadedModel {
    /// Revision of the listing it was loaded from.
    revision: String,
    /// Numbers per vector.
    dimensions: u32,
    /// The tokenizer; its runtime is not safe to share between threads.
    encoder: Mutex<TextEncoder>,
    /// The base rows, then one pack per loaded language.
    packs: Vec<Rows>,
    /// The graph.
    graph: Graph,
}

/// Loads the model a listing describes.
///
/// # Arguments
///
/// * `listing` - What [`inspect`] found.
/// * `environment` - The process's ONNX Runtime.
///
/// # Errors
///
/// When a file changed since the listing or fails its SHA-256, the
/// tokenizer or the graph cannot be set up, or the base rows lack a token
/// every text needs.
pub fn load(listing: &Listing, environment: &Environment) -> Result<LoadedModel, ModelError> {
    let manifest = &listing.manifest;
    let dir = listing.dir.as_path();
    // The description must still be the one listed: every other check
    // hangs on the digests in it.
    if read_capped(
        dir,
        MANIFEST_FILE,
        u64::try_from(MAX_MANIFEST_BYTES)
            .unwrap_or(u64::MAX)
            .saturating_add(1),
    )? != listing.manifest_bytes
    {
        return Err(ModelError::Incomplete(MANIFEST_FILE.to_owned()));
    }
    let row_bytes = manifest.row_bytes();
    let mut packs = vec![Rows::parse(
        read_listed(dir, &manifest.base_rows)?,
        row_bytes,
    )?];
    for tag in &listing.languages {
        if let Some(file) = manifest.languages.get(tag) {
            packs.push(Rows::parse(read_listed(dir, file)?, row_bytes)?);
        }
    }
    let encoder = TextEncoder::new(
        read_listed(dir, &manifest.tokenizer.model)?,
        &manifest.tokenizer,
        manifest.max_tokens,
    )?;
    if let Some(missing) = encoder
        .required_ids()
        .find(|id| packs.first().is_none_or(|base| base.row(*id).is_none()))
    {
        return Err(ModelError::BaseRow(missing));
    }
    let graph = Graph::load(
        environment,
        &read_listed(dir, &manifest.graph.model)?,
        &manifest.graph,
        &manifest.tables,
        manifest.dimensions,
    )?;
    Ok(LoadedModel {
        revision: listing.revision.clone(),
        dimensions: manifest.dimensions,
        encoder: Mutex::new(encoder),
        packs,
        graph,
    })
}

impl LoadedModel {
    /// Revision of the listing the model was loaded from.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// Numbers per vector.
    #[must_use]
    pub fn dimensions(&self) -> u32 {
        self.dimensions
    }

    /// The row of token `id` in the first pack that holds it.
    fn row(&self, id: u32) -> Option<&[u8]> {
        self.packs.iter().find_map(|pack| pack.row(id))
    }

    /// The ids the graph runs on for a text, and whether a row was missing.
    fn resolve(&self, text: &str, role: Role) -> Result<(Vec<u32>, bool), ModelError> {
        let encoder = self.encoder.lock().unwrap_or_else(PoisonError::into_inner);
        let body = encoder.body_ids(text, role)?;
        let mut fallback = false;
        let mut resolved = Vec::with_capacity(body.len());
        for id in body {
            if self.row(id).is_some() {
                resolved.push(id);
                continue;
            }
            fallback = true;
            for spelled in encoder.spell(id) {
                resolved.push(if self.row(spelled).is_some() {
                    spelled
                } else {
                    encoder.unknown_id()
                });
            }
        }
        // Spelling makes a text longer; the frame cuts it at the limit again.
        Ok((encoder.frame(resolved), fallback))
    }

    /// Embeds one text.
    ///
    /// # Arguments
    ///
    /// * `text` - The text.
    /// * `role` - Document or query.
    ///
    /// # Errors
    ///
    /// When the tokenizer or the graph fails, or the output is not finite.
    pub fn embed(&self, text: &str, role: Role) -> Result<Embedding, ModelError> {
        let (ids, fallback) = self.resolve(text, role)?;
        let rows = ids
            .iter()
            .map(|id| self.row(*id).ok_or(ModelError::BaseRow(*id)))
            .collect::<Result<Vec<_>, _>>()?;
        let mut vector = self.graph.run(&rows)?;
        if vector.iter().any(|number| !number.is_finite()) {
            return Err(ModelError::NotFinite);
        }
        let norm = vector
            .iter()
            .map(|number| f64::from(*number).powi(2))
            .sum::<f64>()
            .sqrt();
        if norm > 0.0 {
            for number in &mut vector {
                // Narrowing back to the vector's own precision.
                #[allow(clippy::cast_possible_truncation)]
                let scaled = (f64::from(*number) / norm) as f32;
                *number = scaled;
            }
        }
        Ok(Embedding { vector, fallback })
    }
}
