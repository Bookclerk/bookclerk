//! `bookclerk library upload`: sends files, folders and archives to a
//! running bookclerkd as one upload, the way the web app does.
//!
//! The CLI reads each file's size and SHA-256, opens the upload with its
//! manifest (`POST /api/uploads`), sends each file in resumable `PATCH`
//! requests, commits it, and waits for the ingest job unless told not to.
//! It signs in with the operator token, so the books go to the library's
//! first active Owner. Symbolic links are skipped, never followed.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bookclerk_config::Config;
use bookclerk_tls::ureq;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::commands::daemon_cmd::{daemon_base_url, operator_bearer};

/// The media type of a `PATCH` body.
const OFFSET_STREAM: &str = "application/offset+octet-stream";

/// Attempts per file before the upload gives up on it.
const ATTEMPTS: u32 = 3;

/// How often a waiting CLI re-reads the upload.
const POLL: Duration = Duration::from_secs(1);

/// One local file and its place in the upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalFile {
    /// Its path in the upload (`Folder/File.m4b`, `/`-separated).
    pub upload_path: String,
    /// Where it is on this machine.
    pub local: PathBuf,
    /// Its size in bytes.
    pub size: u64,
}

/// Lists the files `inputs` name: a file as itself, a folder as every
/// regular file beneath it under the folder's own name.
///
/// # Errors
///
/// Returns an error for a missing input, a name that is not UTF-8, or a
/// folder that cannot be read.
pub(crate) fn collect(inputs: &[PathBuf]) -> anyhow::Result<Vec<LocalFile>> {
    let mut out = Vec::new();
    for input in inputs {
        let meta = std::fs::symlink_metadata(input)
            .map_err(|err| anyhow::anyhow!("{}: {err}", input.display()))?;
        let base = input
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("{}: the name is not UTF-8", input.display()))?;
        if meta.is_file() {
            out.push(LocalFile {
                upload_path: base.to_string(),
                local: input.clone(),
                size: meta.len(),
            });
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(input)
            .follow_links(false)
            .sort_by_file_name()
        {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry.path().strip_prefix(input)?;
            let mut parts = vec![base.to_string()];
            for part in relative.components() {
                let part = part.as_os_str().to_str().ok_or_else(|| {
                    anyhow::anyhow!("{}: the name is not UTF-8", entry.path().display())
                })?;
                parts.push(part.to_string());
            }
            out.push(LocalFile {
                upload_path: parts.join("/"),
                local: entry.path().to_path_buf(),
                size: entry.metadata()?.len(),
            });
        }
    }
    Ok(out)
}

/// The lowercase hex SHA-256 of the file at `path`.
fn sha256_of(path: &Path) -> anyhow::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(buf.get(..read).unwrap_or_default());
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Runs `bookclerk library upload`.
///
/// # Arguments
///
/// * `config` - The CLI's configuration (daemon address and token).
/// * `inputs` - Files, folders and archives to upload.
/// * `wait` - Wait for the ingest job and print what became of each book.
///
/// # Errors
///
/// Returns an error when nothing is found to upload, the daemon refuses the
/// upload, or a file cannot be sent.
pub async fn run(config: &Config, inputs: Vec<PathBuf>, wait: bool) -> anyhow::Result<()> {
    let files = collect(&inputs)?;
    anyhow::ensure!(!files.is_empty(), "no files to upload");
    let base = daemon_base_url(config);
    let token = operator_bearer(config).await?;
    tokio::task::spawn_blocking(move || upload(&base, token.as_deref(), &files, wait))
        .await
        .map_err(|err| anyhow::anyhow!("upload join: {err}"))?
}

/// Sends `files` as one upload and commits it.
fn upload(base: &str, token: Option<&str>, files: &[LocalFile], wait: bool) -> anyhow::Result<()> {
    let mut manifest = Vec::with_capacity(files.len());
    for file in files {
        manifest.push(json!({
            "path": file.upload_path,
            "size": file.size,
            "sha256": sha256_of(&file.local)?,
        }));
    }
    let created = post_json(
        &format!("{base}/api/uploads"),
        token,
        &json!({ "files": manifest }),
    )?;
    let id = created
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("the daemon answered without an upload id"))?
        .to_string();
    println!("upload {id}: {} file(s)", files.len());
    for (member_no, file) in files.iter().enumerate() {
        send_file(base, token, &id, member_no, file)?;
        println!("sent {}", file.upload_path);
    }
    let committed = post_json(
        &format!("{base}/api/uploads/{id}/commit"),
        token,
        &json!({}),
    )?;
    let job = committed.get("jobId").and_then(Value::as_str).unwrap_or("");
    println!("committed; ingest job {job}");
    if !wait {
        return Ok(());
    }
    let finished = loop {
        let batch = get_json(&format!("{base}/api/uploads/{id}"), token)?;
        let state = batch.get("state").and_then(Value::as_str).unwrap_or("");
        if matches!(state, "done" | "failed" | "cancelled") {
            break batch;
        }
        std::thread::sleep(POLL);
    };
    print_outcomes(&finished);
    let state = finished.get("state").and_then(Value::as_str).unwrap_or("");
    anyhow::ensure!(
        state == "done",
        "the upload {state}: {}",
        finished
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or("no detail")
    );
    Ok(())
}

/// Prints each file's outcome.
fn print_outcomes(batch: &Value) {
    let files = batch.get("files").and_then(Value::as_array);
    for file in files.into_iter().flatten() {
        let path = file.get("path").and_then(Value::as_str).unwrap_or("");
        let outcome = file.get("outcome").and_then(Value::as_str).unwrap_or("");
        match file.get("reason").and_then(Value::as_str) {
            Some(reason) => println!("{outcome}\t{path}\t{reason}"),
            None => println!("{outcome}\t{path}"),
        }
    }
}

/// Sends one file's bytes, resuming from where the daemon stands after a
/// broken request.
fn send_file(
    base: &str,
    token: Option<&str>,
    id: &str,
    member_no: usize,
    file: &LocalFile,
) -> anyhow::Result<()> {
    let url = format!("{base}/api/uploads/{id}/files/{member_no}");
    let mut offset = 0u64;
    let mut last_error = None;
    for _ in 0..ATTEMPTS {
        let mut handle = std::fs::File::open(&file.local)?;
        handle.seek(SeekFrom::Start(offset))?;
        let mut request = ureq::patch(&url)
            .config()
            .http_status_as_error(false)
            .build()
            .header("Content-Type", OFFSET_STREAM)
            .header("Upload-Offset", offset.to_string());
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        match request.send(ureq::SendBody::from_reader(&mut handle)) {
            Ok(mut response) if response.status().is_success() => {
                let body: Value = response.body_mut().read_json()?;
                if body.get("complete").and_then(Value::as_bool) == Some(true) {
                    return Ok(());
                }
                offset = body
                    .get("received")
                    .and_then(Value::as_u64)
                    .unwrap_or(offset);
            }
            Ok(mut response) => {
                let status = response.status();
                let body: Value = response.body_mut().read_json().unwrap_or(Value::Null);
                let message = body.get("error").and_then(Value::as_str).unwrap_or("");
                last_error = Some(anyhow::anyhow!(
                    "{}: HTTP {status} {message}",
                    file.upload_path
                ));
                if status.as_u16() != 409 {
                    break;
                }
                offset = received_of(base, token, id, member_no)?;
            }
            Err(err) => {
                last_error = Some(anyhow::anyhow!("{}: {err}", file.upload_path));
                offset = received_of(base, token, id, member_no)?;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("{}: not sent", file.upload_path)))
}

/// Bytes the daemon holds of file `member_no` of upload `id`.
fn received_of(base: &str, token: Option<&str>, id: &str, member_no: usize) -> anyhow::Result<u64> {
    let batch = get_json(&format!("{base}/api/uploads/{id}"), token)?;
    batch
        .get("files")
        .and_then(Value::as_array)
        .and_then(|files| files.get(member_no))
        .and_then(|file| file.get("received"))
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            anyhow::anyhow!("the daemon did not say how much of file {member_no} it holds")
        })
}

/// POSTs a JSON body and reads the JSON answer, turning an error status
/// into an error with the daemon's message.
fn post_json(url: &str, token: Option<&str>, body: &Value) -> anyhow::Result<Value> {
    let mut request = ureq::post(url)
        .config()
        .http_status_as_error(false)
        .build()
        .header("Content-Type", "application/json");
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = request
        .send_json(body)
        .map_err(|err| anyhow::anyhow!("daemon POST {url}: {err}"))?;
    let status = response.status();
    let answer: Value = response.body_mut().read_json().unwrap_or(Value::Null);
    anyhow::ensure!(
        status.is_success(),
        "daemon POST {url}: HTTP {status} {}",
        answer.get("error").and_then(Value::as_str).unwrap_or("")
    );
    Ok(answer)
}

/// Reads a JSON answer, turning an error status into an error.
fn get_json(url: &str, token: Option<&str>) -> anyhow::Result<Value> {
    let mut request = ureq::get(url).config().http_status_as_error(false).build();
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = request
        .call()
        .map_err(|err| anyhow::anyhow!("daemon GET {url}: {err}"))?;
    let status = response.status();
    let answer: Value = response.body_mut().read_json().unwrap_or(Value::Null);
    anyhow::ensure!(
        status.is_success(),
        "daemon GET {url}: HTTP {status} {}",
        answer.get("error").and_then(Value::as_str).unwrap_or("")
    );
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_keep_their_name_and_files_stand_alone() {
        let dir = tempfile::tempdir().unwrap();
        let book = dir.path().join("Dune");
        std::fs::create_dir_all(book.join("Disc 1")).unwrap();
        std::fs::write(book.join("Disc 1").join("01.mp3"), b"abc").unwrap();
        std::fs::write(book.join("cover.jpg"), b"x").unwrap();
        let loose = dir.path().join("loose.m4b");
        std::fs::write(&loose, b"abcd").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&loose, book.join("link.m4b")).unwrap();
        let files = collect(&[book, loose]).unwrap();
        let paths: Vec<_> = files
            .iter()
            .map(|f| (f.upload_path.as_str(), f.size))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("Dune/Disc 1/01.mp3", 3),
                ("Dune/cover.jpg", 1),
                ("loose.m4b", 4)
            ]
        );
        assert!(collect(&[dir.path().join("missing")]).is_err());
    }

    #[test]
    fn digests_are_lowercase_hex() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_of(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
