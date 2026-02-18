use std::collections::HashSet;
use std::{collections::HashMap, fs, sync::Mutex, sync::mpsc::Sender};

use crate::image_store::{ImageStore, ImageFile};
use crate::unix_pipe::UnixPipe;
use anyhow::{Result};
use crate::image_store::mem::File as MemFile;

pub struct FileSender {
    // TODO: implement a memory limit, by making this a buffered
    // channel
    sender: Option<Sender<(String, MemFile)>>,
    small_file_sender: Option<Sender<(String, MemFile)>>,
    is_small_file_closed: bool
}

impl FileSender {
    pub fn new(sender: Sender<(String, MemFile)>, small_file_sender: Sender<(String, MemFile)>) -> Self {
        Self {
            sender: Some(sender),
            small_file_sender: Some(small_file_sender),
            is_small_file_closed: false,
        }
    }

    fn close_small_file_sender(&mut self) {
        self.small_file_sender = None;
    }

    pub fn close_sender(&mut self) {
        self.sender = None;
    }
}

impl ImageStore for FileSender {
    type File = MemFile;

    fn create(&mut self, filename: &str) -> Result<Self::File> {
        Ok(MemFile::new_small())
    }

    fn insert(&mut self, filename: impl Into<Box<str>>, output: Self::File) {
        let filename = filename.into();
        if crate::util::is_small_file(&*filename) {
            assert!(self.small_file_sender.is_some());
            let _ = self.small_file_sender.as_ref().unwrap().send((filename.to_string(), output));
        } else {
            if !self.is_small_file_closed {
                self.close_small_file_sender();
                self.is_small_file_closed = true
            }
            let _ = self.sender.as_ref().unwrap().send((filename.to_string(), output));
        }
    }
}
