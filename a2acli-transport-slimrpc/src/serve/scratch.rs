// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

/// A scratch file under the OS temp dir, removed on drop. Avoids adding
/// a `tempfile` dependency for what's otherwise a one-line write+read.
///
/// The path embeds the process id and a per-call counter, so concurrent
/// `cargo test` runs never share (or delete) each other's files.
pub(super) struct ScratchFile(PathBuf);

impl ScratchFile {
    pub(super) fn new(name: &str, contents: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{}-{id}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        Self(path)
    }

    pub(super) fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }

    pub(super) fn as_path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
