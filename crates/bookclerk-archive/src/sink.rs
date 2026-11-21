//! Where extracted files go.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::path::EntryPath;

/// Where an extraction writes its files.
///
/// [`crate::extract`] only ever names admitted [`EntryPath`]s, checked
/// against earlier names, so a sink never sees two paths that clash.
pub trait Sink {
    /// The writer for one file.
    type Writer: Write;

    /// Creates the file at `path`, and any folders it sits in.
    ///
    /// # Arguments
    ///
    /// * `path` - The file to create.
    ///
    /// # Errors
    ///
    /// The I/O error, when the file cannot be created; the entry is then
    /// refused as corrupt and the walk goes on.
    fn create(&mut self, path: &EntryPath) -> io::Result<Self::Writer>;

    /// Removes a file created for an entry that was then refused.
    ///
    /// # Arguments
    ///
    /// * `path` - A file this sink created.
    fn discard(&mut self, path: &EntryPath);
}

/// Writes files under one folder on disk, which should start empty.
///
/// Files are created with `create_new`, so nothing that already exists is
/// opened and no link is followed. Folders are created one component at a time and must be
/// real folders.
#[derive(Debug, Clone)]
pub struct DirSink {
    /// The output folder.
    root: PathBuf,
}

impl DirSink {
    /// A sink writing under `root`.
    ///
    /// # Arguments
    ///
    /// * `root` - An existing, empty folder.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The output folder.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates each folder `path` sits in, refusing anything that is not a
    /// real folder.
    fn make_parents(&self, path: &EntryPath) -> io::Result<PathBuf> {
        let mut dir = self.root.clone();
        let parts: Vec<&str> = path.components().collect();
        let (file, folders) = parts
            .split_last()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty path"))?;
        for part in folders {
            dir.push(part);
            match std::fs::symlink_metadata(&dir) {
                Ok(meta) if meta.file_type().is_dir() => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "a parent is not a folder",
                    ))
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => std::fs::create_dir(&dir)?,
                Err(err) => return Err(err),
            }
        }
        Ok(dir.join(file))
    }
}

impl Sink for DirSink {
    type Writer = File;

    fn create(&mut self, path: &EntryPath) -> io::Result<File> {
        let target = self.make_parents(path)?;
        // `create_new` is `O_CREAT | O_EXCL`, which fails on an existing
        // name, a symbolic link included, rather than following it.
        OpenOptions::new().write(true).create_new(true).open(target)
    }

    fn discard(&mut self, path: &EntryPath) {
        let target = path
            .components()
            .fold(self.root.clone(), |dir, part| dir.join(part));
        // A file that cannot be removed stays until the host removes the
        // whole output folder.
        let _ = std::fs::remove_file(target);
    }
}

/// A shared byte buffer.
type Buffer = Rc<RefCell<Vec<u8>>>;

/// Keeps files in memory, for tests and the fuzz target.
#[derive(Debug, Default, Clone)]
pub struct MemorySink {
    /// Buffers by path, filled by their writers.
    buffers: BTreeMap<EntryPath, Buffer>,
}

/// The writer [`MemorySink`] hands out.
#[derive(Debug)]
pub struct MemoryWriter {
    /// Where the bytes go.
    buffer: Buffer,
}

impl Write for MemoryWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl MemorySink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The files written so far, by path.
    #[must_use]
    pub fn files(&self) -> BTreeMap<EntryPath, Vec<u8>> {
        self.buffers
            .iter()
            .map(|(path, buffer)| (path.clone(), buffer.borrow().clone()))
            .collect()
    }
}

impl Sink for MemorySink {
    type Writer = MemoryWriter;

    fn create(&mut self, path: &EntryPath) -> io::Result<MemoryWriter> {
        if self.buffers.contains_key(path) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "exists"));
        }
        let buffer = Buffer::default();
        self.buffers.insert(path.clone(), buffer.clone());
        Ok(MemoryWriter { buffer })
    }

    fn discard(&mut self, path: &EntryPath) {
        self.buffers.remove(path);
    }
}
