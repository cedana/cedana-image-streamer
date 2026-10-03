// SPDX-License-Identifier: Apache-2.0

use super::{ImageStore, list_filenames};
use anyhow::{Context, Result};
use std::{collections::HashMap, fs, io::Seek};
use nix::{
    fcntl::{fallocate, FallocateFlags},
    sys::memfd::{memfd_create, MFdFlags},
};
use crate::{
    unix_pipe::{UnixPipe, UnixPipeImpl},
    util::{MB, PAGE_SIZE},
};

/// Stores each image in its own memfd. Shard data is spliced directly into the file,
/// which can then be handed to the client or spliced into its pipe.
#[derive(Default)]
pub struct Store {
    files: HashMap<Box<str>, fs::File>,
}

impl Store {
    pub fn remove(&mut self, filename: &str) -> Option<fs::File> {
        self.files.remove(filename)
    }

    pub fn list(&self, pattern: &str) -> Vec<String> {
        list_filenames(self.files.keys().map(|filename| filename.as_ref()), pattern)
    }
}

impl ImageStore for Store {
    type File = fs::File;

    fn create(&mut self, filename: &str) -> Result<Self::File> {
        // The image filename is kept in the map; a fixed memfd name avoids the kernel's
        // shorter name-length limit imposing a restriction on image filenames.
        let fd = memfd_create("cedana-image", MFdFlags::MFD_CLOEXEC)
            .with_context(|| format!("Failed to create memfd for {}", filename))?;
        Ok(fs::File::from(fd))
    }

    fn insert(&mut self, filename: impl Into<Box<str>>, file: Self::File) {
        let filename = filename.into();
        assert!(!self.files.contains_key(&filename), "Image file {} is being overwritten", filename);
        self.files.insert(filename, file);
    }
}

/// Transfers a non-GPU file into the client's pipe, releasing storage as it is sent.
pub fn drain(mut file: fs::File, dst: &mut UnixPipe) -> Result<()> {
    file.rewind()?;
    let size = file.metadata()?.len();
    let mut sent = 0;
    let mut released = 0;

    while sent < size {
        let chunk_size = (size - sent).min(MB as u64) as usize;
        file.splice_all(dst, chunk_size)?;
        sent += chunk_size as u64;

        // Only punch complete pages: zeroing a partial page could modify data still
        // referenced by the client's pipe. Full pages remain alive in the pipe until read.
        let release_end = sent / *PAGE_SIZE as u64 * *PAGE_SIZE as u64;
        if release_end > released {
            fallocate(&file,
                      FallocateFlags::FALLOC_FL_PUNCH_HOLE | FallocateFlags::FALLOC_FL_KEEP_SIZE,
                      released as libc::off_t, (release_end - released) as libc::off_t)
                .context("Failed to release transferred memfd pages")?;
            released = release_end;
        }
    }

    Ok(())
}
