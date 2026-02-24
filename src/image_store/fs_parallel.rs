use std::io::Read;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::cmp::min;

use crate::image_store::{ImageStore, ImageFile};
use crate::mmap_buf::MmapBuf;
use crate::unix_pipe::UnixPipe;
use crate::util;
use crate::semaphore::Semaphore;
use anyhow::{Result};

pub struct FileSender {
    sender: Option<Sender<(String, FileContent)>>,
    small_file_sender: Option<Sender<(String, FileContent)>>,
    is_small_file_closed: bool,
    semaphore: Arc<Semaphore>
}

impl FileSender {
    pub fn new(sender: Sender<(String, FileContent)>, small_file_sender: Sender<(String, FileContent)>, semaphore: Arc<Semaphore>) -> Self {
        Self {
            sender: Some(sender),
            small_file_sender: Some(small_file_sender),
            is_small_file_closed: false,
            semaphore
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
    type File = File;

    fn create(&mut self, filename: &str) -> Result<Self::File> {
        if crate::util::is_small_file(filename) {
            let file_sender = self.small_file_sender.as_ref().cloned().unwrap();
            Ok(File::new(filename.to_string(), Arc::clone(&self.semaphore), file_sender))
        } else {
            let file_sender = self.sender.as_ref().cloned().unwrap();
            if !self.is_small_file_closed {
                self.close_small_file_sender();
                self.is_small_file_closed = true
            }
            Ok(File::new(filename.to_string(), Arc::clone(&self.semaphore), file_sender))
        }
    }

    fn insert(&mut self, filename: impl Into<Box<str>>, _output: Self::File) {
        let filename = filename.into();
        if crate::util::is_small_file(&filename) {
            assert!(self.small_file_sender.is_some());
            let _ = self.small_file_sender.as_ref().unwrap().send((filename.to_string(), FileContent::Eof));
        } else {
            let _ = self.sender.as_ref().unwrap().send((filename.to_string(), FileContent::Eof));
        }
    }
}

#[derive(Debug)]
pub enum FileContent {
    Eof,
    Content(MmapBuf)
}

pub struct File {
    filename: String,
    semaphore: Arc<Semaphore>,
    sender: Sender<(String, FileContent)>
}

impl File {
    fn new(filename: String, semaphore: Arc<Semaphore>, sender: Sender<(String, FileContent)>) -> Self {
        Self { filename, semaphore, sender }
    }
}

impl ImageFile for File {
    fn write_all_from_pipe(&mut self, shard_pipe: &mut UnixPipe, size: usize) -> Result<()> {
        // TODO: maybe make this configurable?
        let chunk_size = 10*util::MB;

        let mut to_send = size;
        while to_send > 0 {
            let size = min(to_send, chunk_size);
            // we release when we send this data out to client
            // semaphore enforces our memory limit for us
            self.semaphore.acquire(size as isize);
            let mut chunk = MmapBuf::with_capacity(size);
            chunk.resize(size);
            shard_pipe.read_exact(&mut chunk)?;
            to_send -= size;
            let _ = self.sender.send((self.filename.to_string(), FileContent::Content(chunk)));
        }
        Ok(())

    }
}
