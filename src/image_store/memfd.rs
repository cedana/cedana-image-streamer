// SPDX-License-Identifier: Apache-2.0

use super::{ImageStore, ImageFile};
use anyhow::{ensure, Context, Result};
use regex::Regex;
use std::{collections::HashMap, io::Seek, os::fd::AsFd};
use memfd::{FileSeal, Memfd, MemfdOptions};
use nix::fcntl::{splice, SpliceFFlags};
use crate::{
    unix_pipe::{UnixPipe, UnixPipeImpl},
    util::MB,
};

/// Stores each image in its own memfd. Shard data is spliced directly into the file,
/// which can then be handed to the client or spliced into its pipe.
#[derive(Default)]
pub struct Store {
    files: HashMap<Box<str>, Memfd>,
}

impl Store {
    pub fn remove(&mut self, filename: &str) -> Option<Memfd> {
        self.files.remove(filename)
    }

    pub fn get(&self, filename: &str) -> Option<&Memfd> {
        self.files.get(filename)
    }

    pub fn list_files(&self, pattern: &str) -> Vec<String> {
        let mut regex_pattern = String::from("^");
        for ch in pattern.chars() {
            match ch {
                '*' => regex_pattern.push_str(".*"),
                '?' => regex_pattern.push('.'),
                '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                    regex_pattern.push('\\');
                    regex_pattern.push(ch);
                }
                _ => regex_pattern.push(ch),
            }
        }
        regex_pattern.push_str(if pattern.is_empty() { ".*$" } else { "$" });

        let Ok(re) = Regex::new(&regex_pattern) else { return Vec::new() };
        self.files.keys()
            .filter(|filename| re.is_match(filename))
            .map(|filename| filename.to_string())
            .collect()
    }
}

impl ImageStore for Store {
    type File = Memfd;

    fn create(&mut self, filename: &str) -> Result<Self::File> {
        MemfdOptions::new().allow_sealing(true).create("cedana-image")
            .with_context(|| format!("Failed to create memfd for {}", filename))
    }

    fn insert(&mut self, filename: impl Into<Box<str>>, file: Self::File) -> Result<()> {
        let filename = filename.into();
        assert!(!self.files.contains_key(&filename), "Image file {} is being overwritten", filename);
        file.as_file().rewind()
            .with_context(|| format!("Failed to rewind memfd for {}", filename))?;
        file.add_seals(&[
            FileSeal::SealShrink,
            FileSeal::SealGrow,
            FileSeal::SealWrite,
            FileSeal::SealSeal,
        ]).with_context(|| format!("Failed to seal memfd for {}", filename))?;
        self.files.insert(filename, file);
        Ok(())
    }
}

impl ImageFile for Memfd {
    fn write_all_from_pipe(&mut self, shard_pipe: &mut UnixPipe, size: usize) -> Result<()> {
        let mut remaining = size;
        while remaining > 0 {
            let written = splice(shard_pipe.as_fd(), None, self.as_file().as_fd(), None,
                                 remaining, SpliceFFlags::SPLICE_F_MORE)
                .context("Failed to splice shard data into memfd")?;
            ensure!(written > 0, "Reached EOF during splice() from shard");
            remaining -= written;
        }
        Ok(())
    }
}

/// Splices the whole file into `dst`. The file stays in the store so it can be served again.
/// We pass an explicit offset to splice() so the file's own offset is left untouched: it is
/// shared with every client that received this memfd via `send_memfd()`.
pub fn send(file: &Memfd, dst: &mut UnixPipe) -> Result<()> {
    let file = file.as_file();
    let len = file.metadata()?.len() as i64;
    let mut offset: i64 = 0;

    while offset < len {
        let chunk_size = (len - offset).min(MB as i64) as usize;
        let written = splice(file.as_fd(), Some(&mut offset), dst.as_fd(), None,
                             chunk_size, SpliceFFlags::SPLICE_F_MORE)
            .context("Failed to splice memfd into client pipe")?;
        ensure!(written > 0, "Reached EOF during splice() from memfd");
    }

    Ok(())
}
