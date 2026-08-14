//! Audible-owned Adrm aaxc + Widevine CENC decrypt.
//!
//! Host acquire never sees ciphertext keys — [`ContentSource::fetch_title`]
//! decrypts here and returns [`bookclerk_source::PlainFetch`].

mod crypto;
mod decrypt;
/// DRM-specific error types (`DrmError`) returned to the Audible source plugin.
mod error;
mod mp4;
mod native;

pub use bookclerk_mp4::TrimRange;
pub use error::{DrmError, Result};
pub use native::{decrypt_adrm_native, decrypt_cenc_native};

use std::path::PathBuf;

/// Input for an Adrm aaxc decrypt job.
#[derive(Debug, Clone)]
pub struct DecryptRequest {
    /// Encrypted aaxc/AAX path that must already exist.
    pub input: PathBuf,
    /// Destination path for the decrypted progressive file (parents are created).
    pub output: PathBuf,
    /// Hex AES-128 key from the voucher; required together with [`Self::audible_iv`].
    pub audible_key: Option<String>,
    /// Hex CBC IV from the voucher; required together with [`Self::audible_key`].
    pub audible_iv: Option<String>,
    /// Legacy AAX activation bytes; this native path rejects them as unsupported.
    pub activation_bytes: Option<String>,
    /// Optional media-time trim applied during remux (chapter/start-end).
    pub trim: Option<TrimRange>,
}

/// Input for a Widevine CENC decrypt job.
#[derive(Debug, Clone)]
pub struct CencDecryptRequest {
    /// Encrypted CENC (DASH or progressive `enca`) path that must already exist.
    pub input: PathBuf,
    /// Destination path for the decrypted file (parents are created).
    pub output: PathBuf,
    /// Widevine key id (hex) used to select the content key.
    pub kid: String,
    /// Widevine content key (hex) for AES-CTR sample decrypt.
    pub key: String,
    /// Optional media-time trim applied during remux.
    pub trim: Option<TrimRange>,
}

/// Outcome of a successful decrypt.
#[derive(Debug, Clone)]
pub struct DecryptOutcome {
    /// Path written on success (same as the request's output).
    pub output: PathBuf,
}

/// Decrypt Adrm aaxc natively (AES-128-CBC sample remux + optional trim).
pub async fn decrypt_adrm(req: DecryptRequest) -> Result<DecryptOutcome> {
    if !req.input.exists() {
        return Err(DrmError::InputMissing(req.input));
    }
    let (Some(key), Some(iv)) = (&req.audible_key, &req.audible_iv) else {
        if req.activation_bytes.is_some() {
            return Err(DrmError::UnsupportedActivationBytes);
        }
        return Err(DrmError::MissingCredentials);
    };
    if let Some(parent) = req.output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let input = req.input.clone();
    let output = req.output.clone();
    let key = key.clone();
    let iv = iv.clone();
    let trim = req.trim;
    tokio::task::spawn_blocking(move || decrypt_adrm_native(&input, &output, &key, &iv, trim))
        .await
        .map_err(|err| DrmError::Native(format!("decrypt task join error: {err}")))?
}

/// Decrypt Widevine CENC natively (fragmented DASH or progressive `enca`).
pub async fn decrypt_cenc(req: CencDecryptRequest) -> Result<DecryptOutcome> {
    if !req.input.exists() {
        return Err(DrmError::InputMissing(req.input.clone()));
    }
    if let Some(parent) = req.output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let input = req.input.clone();
    let output = req.output.clone();
    let kid = req.kid.clone();
    let key = req.key.clone();
    let trim = req.trim;
    tokio::task::spawn_blocking(move || decrypt_cenc_native(&input, &output, &kid, &key, trim))
        .await
        .map_err(|err| DrmError::Native(format!("decrypt task join error: {err}")))?
}
