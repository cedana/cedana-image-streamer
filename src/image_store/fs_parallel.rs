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
    semaphore: Arc<Semaphore>
}

impl FileSender {
    pub fn new(sender: Sender<(String, FileContent)>, small_file_sender: Sender<(String, FileContent)>, semaphore: Arc<Semaphore>) -> Self {
        Self {
            sender: Some(sender),
            small_file_sender: Some(small_file_sender),
            semaphore
        }
    }

    fn close_small_file_sender(&mut self) {
        self.small_file_sender = None;
    }

    pub fn close_senders(&mut self) {
        self.small_file_sender = None;
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
            // as soon as we start receiving large files
            // we should've already received all the small files + metadata
            // therefore we can safely close this sender
            if self.small_file_sender.is_some() {
                self.close_small_file_sender();
            }
            Ok(File::new(filename.to_string(), Arc::clone(&self.semaphore), file_sender))
        }
    }

    fn insert(&mut self, filename: impl Into<Box<str>>, _output: Self::File) -> Result<()> {
        let filename = filename.into();
        if crate::util::is_small_file(&filename) {
            self.small_file_sender
                .as_ref()
                .unwrap()
                .send((filename.to_string(), FileContent::Eof))
                .map_err(|e| anyhow!("could not insert file: {}", e))?;
        } else {
            // by the time, we are inserting large files we should've
            // completely drained the small file shard and therefore
            // closed the small file sender.
            assert!(self.small_file_sender.is_none());
            self.sender
                .as_ref()
                .unwrap()
                .send((filename.to_string(), FileContent::Eof))
                .map_err(|e| anyhow!("could not insert file: {}", e))?;
        }
        Ok(())
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
    sender: Sender<(String, FileContent)>,
    is_small: bool
}

impl File {
    fn new(filename: String, semaphore: Arc<Semaphore>, sender: Sender<(String, FileContent)>) -> Self {
        let is_small = util::is_small_file(&filename);
        Self { filename, semaphore, sender, is_small }
    }
}

impl ImageFile for File {
    fn write_all_from_pipe(&mut self, shard_pipe: &mut UnixPipe, size: usize) -> Result<()> {
        let chunk_size = 10*util::MB;

        let mut to_send = size;
        while to_send > 0 {
            let size = min(to_send, chunk_size);
            // we need to ensure that we have enough memory to load this
            // chunk into memory. This semaphore is used to do this enforcing.
            // When we serve a chunk to Client, we release memory from this
            // semaphore (take a look at serve_img()).
            // we only do this enforcing for large files.
            if !self.is_small {
                self.semaphore.acquire(size as isize);
            }
            let mut chunk = MmapBuf::with_capacity(size);
            chunk.resize(size);
            shard_pipe.read_exact(&mut chunk)?;
            to_send -= size;
            let _ = self.sender.send((self.filename.to_string(), FileContent::Content(chunk)));
        }
        Ok(())

    }
}
