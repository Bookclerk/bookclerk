//! External-sort index for local [`crate::StorageBackend::list_page`].
//!
//! A fresh scan (`cursor == None`) walks the tree once and spills sorted runs
//! of [`INDEX_CHUNK`] keys. Later pages binary-search that file. Memory during
//! the build is one chunk plus a handful of merge heads, not the namespace.
//! The index is node-local scratch under the storage root; it is not a
//! portable checkpoint. A cursor whose key is gone returns
//! [`StorageError::InvalidCursor`] instead of restarting at page one.

#![allow(clippy::missing_docs_in_private_items)]

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Result, StorageError};
use crate::traits::{ListPage, ObjectInfo};

/// Keys sorted in memory before a run is spilled.
pub(crate) const INDEX_CHUNK: usize = 2048;

const MAGIC: &[u8; 4] = b"BCLI";
const VERSION: u32 = 1;
const HEADER_LEN: u64 = 32;
/// Reject index records that would allocate an unbounded key buffer.
const MAX_INDEX_KEY_LEN: usize = 64 * 1024;

#[cfg(test)]
static REBUILD_GATE: std::sync::Mutex<Option<std::sync::Arc<std::sync::Barrier>>> =
    std::sync::Mutex::new(None);

/// Build counters visible across `spawn_blocking` workers.
#[cfg(test)]
static INDEX_BUILDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Largest sort chunk observed by a build.
#[cfg(test)]
static INDEX_MAX_CHUNK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Reads and clears index-build counters (tests only).
#[cfg(test)]
pub(crate) fn take_index_stats() -> (u64, usize) {
    (
        INDEX_BUILDS.swap(0, std::sync::atomic::Ordering::Relaxed),
        INDEX_MAX_CHUNK.swap(0, std::sync::atomic::Ordering::Relaxed),
    )
}

#[cfg(test)]
fn note_build() {
    INDEX_BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(test))]
fn note_build() {}

#[cfg(test)]
fn note_chunk(len: usize) {
    INDEX_MAX_CHUNK.fetch_max(len, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(test))]
fn note_chunk(len: usize) {
    let _ = len;
}

/// One page from the on-disk index.
pub(crate) fn list_page_indexed(
    root: &Path,
    storage_prefix: &str,
    list_prefix: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<ListPage> {
    let path = index_path(root, storage_prefix, list_prefix);
    if cursor.is_none() {
        rebuild_index(root, storage_prefix, list_prefix, &path)?;
    } else if !path.is_file() {
        return Err(StorageError::InvalidCursor(
            "list index is missing for this cursor".into(),
        ));
    }
    if let Some(cursor) = cursor {
        if !cursor_key_exists(root, storage_prefix, cursor) {
            return Err(StorageError::InvalidCursor(
                "stale or unknown list cursor".into(),
            ));
        }
    }
    read_page(&path, cursor, limit)
}

fn cursor_key_exists(root: &Path, storage_prefix: &str, cursor: &str) -> bool {
    let mut full = root.to_path_buf();
    if !storage_prefix.is_empty() {
        full.push(storage_prefix.trim_end_matches('/'));
    }
    for part in cursor.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return false;
        }
        full.push(part);
    }
    full.is_file()
}

fn index_path(root: &Path, storage_prefix: &str, list_prefix: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(storage_prefix.as_bytes());
    hasher.update([0]);
    hasher.update(list_prefix.as_bytes());
    let name = hex::encode(&hasher.finalize()[..8]);
    root.join(".bookclerk-list-index")
        .join(format!("{name}.idx"))
}

fn rebuild_index(
    root: &Path,
    storage_prefix: &str,
    list_prefix: &str,
    final_path: &Path,
) -> Result<()> {
    note_build();
    let index_root = prepare_index_root(root)?;
    sweep_abandoned_builds(&index_root)?;
    let build_id = uuid::Uuid::new_v4();
    let build_dir = index_root.join("builds").join(build_id.to_string());
    fs::create_dir_all(&build_dir)?;
    let lock_path = build_dir.join("active.lock");
    let build_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    fs4::FileExt::lock(&build_lock)?;
    let generation = claim_generation(&generation_path(final_path))?;
    wait_for_test_gate();
    let published = (|| {
        let runs = spill_runs(root, storage_prefix, list_prefix, &build_dir)?;
        let building = build_dir.join("index.building");
        merge_runs(&runs, &building)?;
        for run in &runs {
            let _ = fs::remove_file(run);
        }
        ensure_published(&index_root, &build_dir, final_path, &building, generation)
    })();
    let _ = fs4::FileExt::unlock(&build_lock);
    drop(build_lock);
    let _ = fs::remove_dir_all(&build_dir);
    published
}

fn wait_for_test_gate() {
    #[cfg(test)]
    {
        let gate = REBUILD_GATE.lock().ok().and_then(|guard| guard.clone());
        if let Some(gate) = gate {
            gate.wait();
        }
    }
}

fn prepare_index_root(root: &Path) -> Result<PathBuf> {
    let dir = root.join(".bookclerk-list-index");
    if fs::symlink_metadata(&dir)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(StorageError::InvalidKey(
            "list index directory is a symlink".into(),
        ));
    }
    fs::create_dir_all(&dir)?;
    if fs::symlink_metadata(&dir)?.file_type().is_symlink() {
        return Err(StorageError::InvalidKey(
            "list index directory is a symlink".into(),
        ));
    }
    let canon_root = fs::canonicalize(root)?;
    let canon_dir = fs::canonicalize(&dir)?;
    if !canon_dir.starts_with(&canon_root) {
        return Err(StorageError::InvalidKey(
            "list index directory escapes the storage root".into(),
        ));
    }
    Ok(dir)
}

fn generation_path(final_path: &Path) -> PathBuf {
    let name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "index.idx".into());
    final_path.with_file_name(format!("{name}.gen"))
}

fn claim_generation(path: &Path) -> Result<u64> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    fs4::FileExt::lock(&file)?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    let current = text.trim().parse::<u64>().unwrap_or(0);
    let next = current.saturating_add(1);
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    write!(file, "{next}")?;
    file.sync_all()?;
    fs4::FileExt::unlock(&file)?;
    Ok(next)
}

fn publish_if_current(final_path: &Path, building: &Path, generation: u64) -> Result<bool> {
    let gen_path = generation_path(final_path);
    let mut file = OpenOptions::new().read(true).write(true).open(&gen_path)?;
    fs4::FileExt::lock(&file)?;
    let current = read_generation(&mut file)?;
    let published = if current == generation {
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(building, final_path)?;
        true
    } else {
        false
    };
    fs4::FileExt::unlock(&file)?;
    Ok(published)
}

fn read_generation(file: &mut File) -> Result<u64> {
    file.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text.trim().parse::<u64>().unwrap_or(0))
}

/// Publish `generation` when it is still current. Otherwise wait until a newer
/// build publishes, or publish this complete build when no other build is alive
/// and the index file is still missing.
fn ensure_published(
    index_root: &Path,
    build_dir: &Path,
    final_path: &Path,
    building: &Path,
    generation: u64,
) -> Result<()> {
    if publish_if_current(final_path, building, generation)? || final_path.is_file() {
        return Ok(());
    }
    for _ in 0..100_000 {
        if final_path.is_file() {
            return Ok(());
        }
        if other_builds_active(index_root, build_dir)? {
            std::thread::yield_now();
            continue;
        }
        let gen_path = generation_path(final_path);
        let file = OpenOptions::new().read(true).write(true).open(&gen_path)?;
        fs4::FileExt::lock(&file)?;
        if final_path.is_file() {
            fs4::FileExt::unlock(&file)?;
            return Ok(());
        }
        if other_builds_active(index_root, build_dir)? {
            fs4::FileExt::unlock(&file)?;
            std::thread::yield_now();
            continue;
        }
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(building, final_path)?;
        fs4::FileExt::unlock(&file)?;
        return Ok(());
    }
    Err(StorageError::Other(anyhow::anyhow!(
        "list index publication did not finish"
    )))
}

fn other_builds_active(index_root: &Path, ours: &Path) -> Result<bool> {
    let builds = index_root.join("builds");
    let Ok(entries) = fs::read_dir(&builds) else {
        return Ok(false);
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path == ours || !path.is_dir() {
            continue;
        }
        let lock_path = path.join("active.lock");
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
            continue;
        };
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => {
                let _ = fs4::FileExt::unlock(&file);
            }
            Err(_) => return Ok(true),
        }
    }
    Ok(false)
}

fn sweep_abandoned_builds(index_root: &Path) -> Result<()> {
    let builds = index_root.join("builds");
    let Ok(entries) = fs::read_dir(&builds) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let lock_path = path.join("active.lock");
        let Some(file) = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .ok()
        else {
            let _ = fs::remove_dir_all(&path);
            continue;
        };
        if let Ok(()) = fs4::FileExt::try_lock(&file) {
            let _ = fs4::FileExt::unlock(&file);
            drop(file);
            let _ = fs::remove_dir_all(&path);
        }
    }
    Ok(())
}

fn spill_runs(
    root: &Path,
    storage_prefix: &str,
    list_prefix: &str,
    dir: &Path,
) -> Result<Vec<PathBuf>> {
    let mut stack = vec![root.to_path_buf()];
    let mut chunk: Vec<(String, u64)> = Vec::with_capacity(INDEX_CHUNK);
    let mut runs = Vec::new();
    let want_prefix = {
        let mut full = String::new();
        if !storage_prefix.is_empty() {
            full.push_str(storage_prefix);
        }
        full.push_str(list_prefix);
        full
    };
    while let Some(dir_path) = stack.pop() {
        let read = match fs::read_dir(&dir_path) {
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(StorageError::Io(err)),
        };
        for entry in read {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if is_internal_name(name) {
                continue;
            }
            let path = entry.path();
            if !path.starts_with(root) {
                continue;
            }
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let rel = match path.strip_prefix(root) {
                Ok(rel) => rel,
                Err(_) => continue,
            };
            let key_full = rel.to_string_lossy().replace('\\', "/");
            if !want_prefix.is_empty() && !key_full.starts_with(&want_prefix) {
                continue;
            }
            let key = if storage_prefix.is_empty() {
                key_full
            } else {
                match key_full.strip_prefix(storage_prefix) {
                    Some(rest) => rest.to_string(),
                    None => continue,
                }
            };
            if !list_prefix.is_empty() && !key.starts_with(list_prefix) {
                continue;
            }
            chunk.push((key, meta.len()));
            note_chunk(chunk.len());
            if chunk.len() >= INDEX_CHUNK {
                runs.push(write_run(dir, runs.len(), &mut chunk)?);
            }
        }
    }
    if !chunk.is_empty() {
        runs.push(write_run(dir, runs.len(), &mut chunk)?);
    }
    Ok(runs)
}

fn is_internal_name(name: &str) -> bool {
    name == ".bookclerk-list-index"
        || name == ".bookclerk-stage"
        || name == ".bookclerk-orphans"
        || (name.starts_with('.') && name.contains(".bookclerk-tmp-"))
}

fn write_run(dir: &Path, index: usize, chunk: &mut Vec<(String, u64)>) -> Result<PathBuf> {
    chunk.sort_by(|a, b| a.0.cmp(&b.0));
    let path = dir.join(format!("run-{index}.bin"));
    let mut file = BufWriter::new(File::create(&path)?);
    for (key, size) in chunk.drain(..) {
        write_record(&mut file, &key, size)?;
    }
    file.flush()?;
    Ok(path)
}

fn write_record(out: &mut impl Write, key: &str, size: u64) -> Result<()> {
    let bytes = key.as_bytes();
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    if bytes.len() > len as usize {
        return Err(StorageError::InvalidKey(format!(
            "storage key exceeds index record: {key}"
        )));
    }
    out.write_all(&len.to_le_bytes())?;
    out.write_all(bytes)?;
    out.write_all(&size.to_le_bytes())?;
    Ok(())
}

fn read_record(input: &mut impl Read) -> Result<Option<(String, u64)>> {
    let mut len_buf = [0u8; 4];
    match input.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(StorageError::Io(err)),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_INDEX_KEY_LEN {
        return Err(StorageError::InvalidCursor(format!(
            "list index record length {len} exceeds {MAX_INDEX_KEY_LEN}"
        )));
    }
    let mut key_buf = vec![0u8; len];
    input.read_exact(&mut key_buf)?;
    let key = String::from_utf8(key_buf)
        .map_err(|_| StorageError::InvalidCursor("list index contains a non-utf8 key".into()))?;
    let mut size_buf = [0u8; 8];
    input.read_exact(&mut size_buf)?;
    Ok(Some((key, u64::from_le_bytes(size_buf))))
}

struct MergeHead {
    key: String,
    size: u64,
    run: usize,
}

impl PartialEq for MergeHead {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.run == other.run
    }
}

impl Eq for MergeHead {}

impl PartialOrd for MergeHead {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MergeHead {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; invert so the smallest key pops first.
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.run.cmp(&self.run))
    }
}

fn merge_runs(runs: &[PathBuf], dest: &Path) -> Result<()> {
    const FANIN: usize = 16;
    if runs.is_empty() {
        return write_empty_index(dest);
    }
    if runs.len() == 1 {
        return transcribe_run(&runs[0], dest);
    }
    let mut owned_intermediates = false;
    let mut current: Vec<PathBuf> = runs.to_vec();
    let mut pass = 0usize;
    while current.len() > FANIN {
        let mut next = Vec::new();
        for (group_index, group) in current.chunks(FANIN).enumerate() {
            let out = dest.with_extension(format!("raw{pass}-{group_index}"));
            merge_group_raw(group, &out)?;
            next.push(out);
        }
        if owned_intermediates {
            for path in &current {
                let _ = fs::remove_file(path);
            }
        }
        current = next;
        owned_intermediates = true;
        pass += 1;
    }
    merge_group(&current, dest)?;
    if owned_intermediates {
        for path in &current {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn merge_group_raw(group: &[PathBuf], dest: &Path) -> Result<()> {
    let mut readers: Vec<BufReader<File>> = open_readers(group)?;
    let mut heap = seed_heap(&mut readers)?;
    let mut out = BufWriter::new(File::create(dest)?);
    while let Some(head) = heap.pop() {
        write_record(&mut out, &head.key, head.size)?;
        if let Some((key, size)) = read_record(&mut readers[head.run])? {
            heap.push(MergeHead {
                key,
                size,
                run: head.run,
            });
        }
    }
    out.flush()?;
    Ok(())
}

fn open_readers(group: &[PathBuf]) -> Result<Vec<BufReader<File>>> {
    group
        .iter()
        .map(File::open)
        .collect::<std::io::Result<Vec<_>>>()
        .map(|files| files.into_iter().map(BufReader::new).collect())
        .map_err(StorageError::Io)
}

fn seed_heap(readers: &mut [BufReader<File>]) -> Result<BinaryHeap<MergeHead>> {
    let mut heap = BinaryHeap::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some((key, size)) = read_record(reader)? {
            heap.push(MergeHead { key, size, run });
        }
    }
    Ok(heap)
}

fn write_empty_index(dest: &Path) -> Result<()> {
    let mut file = File::create(dest)?;
    write_header(&mut file, 0, HEADER_LEN)?;
    Ok(())
}

fn transcribe_run(run: &Path, dest: &Path) -> Result<()> {
    let mut input = BufReader::new(File::open(run)?);
    let mut records = Vec::new();
    while let Some(record) = read_record(&mut input)? {
        records.push(record);
        if records.len() > INDEX_CHUNK {
            return Err(StorageError::Other(anyhow::anyhow!(
                "list index run exceeded {INDEX_CHUNK} keys"
            )));
        }
    }
    stream_records(dest, records.into_iter().map(Ok))
}

fn merge_group(group: &[PathBuf], dest: &Path) -> Result<()> {
    let mut readers = open_readers(group)?;
    let mut heap = seed_heap(&mut readers)?;
    let offsets_path = dest.with_extension("offs");
    let mut data = File::create(dest)?;
    write_header(&mut data, 0, 0)?;
    let mut offsets = BufWriter::new(File::create(&offsets_path)?);
    let mut count = 0u64;
    while let Some(head) = heap.pop() {
        let pos = data.stream_position()?;
        offsets.write_all(&pos.to_le_bytes())?;
        write_record(&mut data, &head.key, head.size)?;
        count += 1;
        if let Some((key, size)) = read_record(&mut readers[head.run])? {
            heap.push(MergeHead {
                key,
                size,
                run: head.run,
            });
        }
    }
    offsets.flush()?;
    drop(offsets);
    finish_offset_table(&mut data, count, &offsets_path)?;
    let _ = fs::remove_file(&offsets_path);
    Ok(())
}

fn stream_records(dest: &Path, records: impl Iterator<Item = Result<(String, u64)>>) -> Result<()> {
    let offsets_path = dest.with_extension("offs");
    let mut data = File::create(dest)?;
    write_header(&mut data, 0, 0)?;
    let mut offsets = BufWriter::new(File::create(&offsets_path)?);
    let mut count = 0u64;
    for record in records {
        let (key, size) = record?;
        let pos = data.stream_position()?;
        offsets.write_all(&pos.to_le_bytes())?;
        write_record(&mut data, &key, size)?;
        count += 1;
    }
    offsets.flush()?;
    drop(offsets);
    finish_offset_table(&mut data, count, &offsets_path)?;
    let _ = fs::remove_file(&offsets_path);
    Ok(())
}

fn finish_offset_table(data: &mut File, count: u64, offsets_path: &Path) -> Result<()> {
    let offsets_pos = data.stream_position()?;
    let mut offsets = File::open(offsets_path)?;
    std::io::copy(&mut offsets, data)?;
    data.seek(SeekFrom::Start(0))?;
    write_header(data, count, offsets_pos)?;
    Ok(())
}

fn write_header(file: &mut File, count: u64, offsets_pos: u64) -> Result<()> {
    file.write_all(MAGIC)?;
    file.write_all(&VERSION.to_le_bytes())?;
    file.write_all(&count.to_le_bytes())?;
    file.write_all(&offsets_pos.to_le_bytes())?;
    file.write_all(&0u64.to_le_bytes())?;
    Ok(())
}

struct IndexFile {
    file: File,
    count: u64,
    offsets_pos: u64,
}

impl IndexFile {
    fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(StorageError::InvalidCursor(
                "list index magic mismatch".into(),
            ));
        }
        let mut buf4 = [0u8; 4];
        file.read_exact(&mut buf4)?;
        if u32::from_le_bytes(buf4) != VERSION {
            return Err(StorageError::InvalidCursor(
                "list index version mismatch".into(),
            ));
        }
        let mut buf8 = [0u8; 8];
        file.read_exact(&mut buf8)?;
        let count = u64::from_le_bytes(buf8);
        file.read_exact(&mut buf8)?;
        let offsets_pos = u64::from_le_bytes(buf8);
        Ok(Self {
            file,
            count,
            offsets_pos,
        })
    }

    fn record_at(&mut self, index: u64) -> Result<(String, u64)> {
        if index >= self.count {
            return Err(StorageError::InvalidCursor(
                "list index offset out of range".into(),
            ));
        }
        self.file
            .seek(SeekFrom::Start(self.offsets_pos + index * 8))?;
        let mut buf8 = [0u8; 8];
        self.file.read_exact(&mut buf8)?;
        let pos = u64::from_le_bytes(buf8);
        self.file.seek(SeekFrom::Start(pos))?;
        read_record(&mut self.file)?
            .ok_or_else(|| StorageError::InvalidCursor("list index record truncated".into()))
    }

    fn find_key(&mut self, cursor: &str) -> Result<u64> {
        let mut lo = 0u64;
        let mut hi = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (key, _) = self.record_at(mid)?;
            match key.as_str().cmp(cursor) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Ok(mid),
            }
        }
        Err(StorageError::InvalidCursor(
            "stale or unknown list cursor".into(),
        ))
    }
}

fn read_page(path: &Path, cursor: Option<&str>, limit: usize) -> Result<ListPage> {
    let mut index = IndexFile::open(path)?;
    let start = if let Some(cursor) = cursor {
        index.find_key(cursor)?.saturating_add(1)
    } else {
        0
    };
    let mut objects = Vec::with_capacity(limit.min(256));
    let mut next_index = start;
    while objects.len() <= limit && next_index < index.count {
        let (key, size) = index.record_at(next_index)?;
        if let Some(cursor) = cursor {
            if key.as_str() <= cursor {
                return Err(StorageError::InvalidCursor(
                    "list cursor did not advance".into(),
                ));
            }
        }
        objects.push(ObjectInfo { key, size });
        next_index += 1;
    }
    let next_cursor = if objects.len() > limit {
        objects.pop();
        objects.last().map(|obj| obj.key.clone())
    } else {
        None
    };
    Ok(ListPage {
        objects,
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn write_keys(root: &Path, keys: &[&str]) {
        for key in keys {
            let path = root.join(key);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, b"x").unwrap();
        }
    }

    fn keys_of(page: &ListPage) -> Vec<String> {
        page.objects.iter().map(|obj| obj.key.clone()).collect()
    }

    #[test]
    fn concurrent_scans_keep_identical_and_distinct_prefixes_isolated() {
        let root = tempfile::tempdir().unwrap();
        write_keys(
            root.path(),
            &[
                "same/a.txt",
                "same/b.txt",
                "same/c.txt",
                "other/z.txt",
                "left/one.txt",
                "right/two.txt",
            ],
        );
        let barrier = Arc::new(Barrier::new(4));
        *REBUILD_GATE.lock().unwrap() = Some(Arc::clone(&barrier));
        let root_path = root.path().to_path_buf();
        let mut joins = Vec::new();
        for prefix in ["same/", "same/", "left/", "right/"] {
            let root_path = root_path.clone();
            let prefix = prefix.to_string();
            joins.push(thread::spawn(move || {
                list_page_indexed(&root_path, "", &prefix, None, 100).unwrap()
            }));
        }
        let pages: Vec<_> = joins.into_iter().map(|join| join.join().unwrap()).collect();
        *REBUILD_GATE.lock().unwrap() = None;
        assert_eq!(
            keys_of(&pages[0]),
            vec!["same/a.txt", "same/b.txt", "same/c.txt"]
        );
        assert_eq!(keys_of(&pages[1]), keys_of(&pages[0]));
        assert_eq!(keys_of(&pages[2]), vec!["left/one.txt"]);
        assert_eq!(keys_of(&pages[3]), vec!["right/two.txt"]);
        let builds = root.path().join(".bookclerk-list-index").join("builds");
        let leftover = fs::read_dir(&builds).map(|rd| rd.count()).unwrap_or(0);
        assert_eq!(
            leftover, 0,
            "finished builds remove their scratch directories"
        );
    }

    #[test]
    fn sweep_removes_abandoned_build_scratch_and_keeps_a_locked_one() {
        let root = tempfile::tempdir().unwrap();
        let index = root.path().join(".bookclerk-list-index");
        let abandoned = index.join("builds").join("abandoned");
        let active = index.join("builds").join("active");
        fs::create_dir_all(&abandoned).unwrap();
        fs::create_dir_all(&active).unwrap();
        fs::write(abandoned.join("run-0.bin"), b"stale").unwrap();
        let lock_path = active.join("active.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        fs4::FileExt::lock(&lock).unwrap();
        fs::write(active.join("run-0.bin"), b"live").unwrap();
        sweep_abandoned_builds(&index).unwrap();
        assert!(!abandoned.exists());
        assert!(active.join("run-0.bin").is_file());
        drop(lock);
    }

    #[test]
    fn malformed_record_length_is_rejected() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&u32::MAX.to_le_bytes());
        let err = read_record(&mut raw.as_slice()).unwrap_err();
        assert!(matches!(err, StorageError::InvalidCursor(_)), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_index_directory_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join(".bookclerk-list-index"))
            .unwrap();
        let err = list_page_indexed(root.path(), "", "", None, 10).unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)), "{err}");
    }
}
