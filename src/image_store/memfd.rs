// SPDX-License-Identifier: Apache-2.0

use super::ImageStore;
use anyhow::{Context, Result};
use regex::Regex;
use std::{collections::HashMap, fs, io::Seek};
use nix::sys::memfd::{memfd_create, MFdFlags};
use crate::{
    unix_pipe::{UnixPipe, UnixPipeImpl},
    util::MB,
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

    pub fn get(&self, filename: &str) -> Option<&fs::File> {
        self.files.get(filename)
    }

    /// Returns the filenames matching the glob `pattern`. `*` and `?` are the only
    /// wildcards. An empty pattern matches everything, an invalid pattern matches nothing.
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

/// Transfers a non-GPU file into the client's pipe. The memfd is freed when `file` is
/// dropped on return; pages still referenced by the pipe stay alive until the client reads them.
pub fn drain(mut file: fs::File, dst: &mut UnixPipe) -> Result<()> {
    file.rewind()?;
    let mut remaining = file.metadata()?.len();

    while remaining > 0 {
        let chunk_size = remaining.min(MB as u64) as usize;
        file.splice_all(dst, chunk_size)?;
        remaining -= chunk_size as u64;
    }

    Ok(())
}
