//  Copyright 2024 Cedana.
//
//  Modifications licensed under the Apache License, Version 2.0.

//  Copyright 2020 Two Sigma Investments, LP.
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.

use std::{
    collections::{BinaryHeap, HashMap, HashSet, VecDeque}, fs, io::{Read, Write}, os::{fd::{AsRawFd, RawFd}, unix::io::AsFd}, path::Path, sync::{Arc, mpsc::{self, Receiver, TryRecvError}}, thread, time::Instant
};
use crate::{
    connection::{Connection, Listener}, criu::FileStatus, image::{self, marker}, image_patcher::patch_img, image_store::{self, ImageFile, ImageStore, fs_overlay, fs_parallel::{self, FileContent}}, impl_ord_by, poller::Poller, semaphore::{self, Semaphore}, unix_pipe::{UnixPipe, UnixPipeImpl}, util::{self, *} 
};
use crate::mmap_buf::MmapBuf;
use nix::{poll::{PollFd, PollFlags, PollTimeout, poll}, sys::{epoll::EpollFlags, sysinfo::sysinfo}};
use anyhow::{Result, Context};

// The serialized image is received via multiple data streams (`Shard`). The data streams are
// comprised of markers followed by an optional data payload. The format of the markers is
// described in ../proto/image.proto.
// Each marker has a sequence number providing a reassembly order (produced in capture.rs).
// At any point in time, a given shard has the marker that should be processed next.
// We represent this with a PendingMarker. Reassembling the stream of markers provides the
// original image files.
//
// When extracting the image, we either store the image files in memory, or write them on disk.
// The former is useful when streaming to client directly, the latter is useful to extract an image
// on disk.
//
// Streaming to client is done by buffering the entire image in memory, and let client consume it.
// XXX Performance isn't that great due to the memory copy in our address space. To improve
// performance, we could splice() shard pipe data to client directly. This is difficult as client
// doesn't read the image files in the same order as they are produced. For example, inventory.img
// is written last in the image, but is read first. One way to go around this issue is to reserve
// a shard during capture for all small image files (pretty much all except pages, ghost files, and
// fs.tar). In addition, we might have to rewrite some part of client to restore these large files in
// the same order as they were produced. It might be difficult to preserve this guarantee forever,
// so it would be wise to keep our in-memory buffering implementation anyways.

/// We are not doing zero-copy transfers to client (yet), we have to be mindful of CPU caches.
/// If we were doing shard to client splices, we could bump the capacity to 4MB.
#[allow(clippy::identity_op)]
const CLIENT_PIPE_DESIRED_CAPACITY: i32 = 1*MB as i32;

/// Data comes in a stream of chunks, which can be as large as 256KB (from capture.rs).
/// We use 512KB to have two chunks in to avoid stalling the shards.
/// Making this buffer bigger would most likely trash CPU caches.
const SHARD_PIPE_DESIRED_CAPACITY: i32 = 512*KB as i32;

struct Shard {
    pipe: UnixPipe,
    transfer_duration_millis: u128,
    bytes_read: u64,
}

impl Shard {
    fn new(mut pipe: UnixPipe) -> Self {
        // Try setting the pipe capacity. Failing is okay, it's just for better performance.
        let _ = pipe.set_capacity(SHARD_PIPE_DESIRED_CAPACITY);
        Self { pipe, bytes_read: 0, transfer_duration_millis: 0 }
    }
}

struct PendingMarker<'a> {
    marker: image::Marker,
    shard: &'a mut Shard,
}

// This gives ordering of the PendingMarker for the BinaryHeap (max-heap). Lowest `seq` comes first,
// hence the `reverse()`. Note that sequence numbers are unique, giving us a total order.
impl_ord_by!(PendingMarker<'a>, |a: &Self, b: &Self| a.marker.seq.cmp(&b.marker.seq).reverse());

struct ImageDeserializer<'a, ImgStore: ImageStore> {
    // Shards are located in three different collections:
    // 1) `shards` stores shards that may not be readable yet. `poll()` is used to determine when a
    //    shard is readable, at which point it is moved to the `readable_shard` vec.
    // 2) `readable_shards` stores shards that are definitely readable. When reading a marker from
    //    a shard, its sequence number is examined and the pair (marker, shard) denoted by the type
    //    `PendingMarker` is placed into the `pending_markers` binary heap.
    // 3) `pending_markers` stores a sorted collection of these pending markers by sequence number.
    //    Once a marker matches the sequence number that we need (stored in the `seq` field), it is
    //    processed with its associated shard. Once processed, the shard goes back in the shards
    //    vec, and the cycle continues.
    small_file_shard: &'a mut Shard,
    small_file_seq: u64,
    metadata: Option<HashMap<Box<str>, usize>>,
    shards: Vec<&'a mut Shard>,
    readable_shards: Vec<&'a mut Shard>,
    pending_markers: BinaryHeap<PendingMarker<'a>>,
    seq: u64,

    // The following fields relate to the output.
    // We use a `Box<str>` instead of `String` for filenames to reduce memory usage by 8 bytes per
    // filenames (`str` are not resizable, `Strings` are, so they need to carry additional information).
    img_store: &'a mut ImgStore,
    // current_img_file we are reading, when we recieve FileEof marker we put it into the ImgStore
    current_img_file: Option<(Box<str>, ImgStore::File)>,

    // `start_time` is used for stats, image_eof is used for safety checks.
    start_time: Instant,
    image_eof: bool,
}

impl<'a, ImgStore: ImageStore> ImageDeserializer<'a, ImgStore> {
    pub fn new(img_store: &'a mut ImgStore, shards: &'a mut [Shard]) -> Self {
        let num_shards = shards.len();
        assert!(num_shards >= 2);
        let mut shard_iter = shards.iter_mut();
        Self {
            small_file_shard: shard_iter.nth(0).unwrap(),
            small_file_seq: 0,
            metadata: None,
            shards: shard_iter.collect(),
            readable_shards: Vec::with_capacity(num_shards - 1),
            pending_markers: BinaryHeap::with_capacity(num_shards - 1),
            seq: 0,
            img_store,
            current_img_file: None,
            start_time: Instant::now(),
            image_eof: false,
        }
    }

    fn mark_image_eof(&mut self) -> Result<()> {
        ensure!(self.current_img_file.is_none() && self.pending_markers.is_empty(),
                "Image EOF marker came unexpectedly");

        self.image_eof = true;
        self.current_img_file = None;
        Ok(())
    }

    fn process_marker(&mut self, marker: image::Marker, shard: &mut Shard) -> Result<()> {
        use marker::Body::*;

        match marker.body {
            Some(Filename(filename)) => {
                assert!(self.current_img_file.is_none());
                let img_file = self.img_store.create(&filename)?;
                self.current_img_file = Some((filename.into_boxed_str(), img_file));
            }
            Some(FileData(size)) => {
                let (_filename, img_file) = self.current_img_file.as_mut()
                    .ok_or_else(|| anyhow!("Unexpected FileData marker"))?;
                img_file.write_all_from_pipe(&mut shard.pipe, size as usize)?;
                shard.bytes_read += size as u64;
            }
            Some(FileEof(true)) => {
                let (filename, img_file) = self.current_img_file.take()
                    .ok_or_else(|| anyhow!("Unexpected FileEof marker"))?;
                self.img_store.insert(filename, img_file);
            }
            Some(ImageEof(true)) => {
                self.mark_image_eof()?;
            }
            _ => bail!("Malformed image marker"),
        }

        Ok(())
    }

    fn get_next_in_order_marker(&mut self) -> Option<PendingMarker<'a>> {
        if let Some(pmarker) = self.pending_markers.peek() {
            if pmarker.marker.seq == self.seq {
                return self.pending_markers.pop();
            }
        }
        None
    }

    fn process_pending_markers(&mut self) -> Result<()> {
        while let Some(PendingMarker { marker, shard }) = self.get_next_in_order_marker() {
            self.process_marker(marker, shard)?;
            self.seq += 1;
            self.shards.push(shard);
        }
        Ok(())
    }

    fn mark_shard_eof(&self, shard: &mut Shard) {
        shard.transfer_duration_millis = self.start_time.elapsed().as_millis();
    }

    fn drain_shard(&mut self, shard: &'a mut Shard) -> Result<()> {
        match pb_read_next(&mut shard.pipe)? {
            None => {
                // EOF of that shard is reached
                self.mark_shard_eof(shard);
            }
            Some((marker, marker_size)) => {
                ensure!(!self.image_eof, "Unexpected data after image EOF");
                shard.bytes_read += marker_size as u64;
                self.pending_markers.push(PendingMarker { marker, shard });
                self.process_pending_markers()?;
            }
        }
        Ok(())
    }

    fn get_next_readable_shard(&mut self) -> Result<Option<&'a mut Shard>> {
        // If we just return `self.shard.pop()`, we may deadlock if shard pipes are not independent
        // from each other.
        // This scenario only happens when the capture shards are directly connected to the extract
        // shards, useful when doing live migrations. The deadlock may happen when the capture
        // serializer attempts to push a large chunk down a shard, and blocks because the shard is
        // full. Meanwhile, the extract reader could be blocking on reading from an empty pipe
        // shard in `pb_read_next()`.
        // To tolerate these workloads, we need to read from shards that are guaranteed to have
        // available data.
        // We use poll() instead of epoll() because we need to ignore the shards that are in the
        // list of pending markers, and we are not doing async reads to do edge triggers.
        if self.readable_shards.is_empty() {
            if self.shards.len() <= 1 {
                // If we have no shard to read from, we'll return None.
                // If we have a single shard to read from, there no need to block in poll()
                // We return immediately with that shard, even if it is not readable yet as it
                // won't introduce a deadlock with the capture side.
                return Ok(self.shards.pop());
            }

            // Collect which shards are readable using poll(). We need to scope the poll_fds
            // borrow so we can mutate self.shards afterward.
            let readable_indices: Vec<usize> = {
                let mut poll_fds: Vec<PollFd> = self.shards.iter()
                    .map(|shard| PollFd::new(shard.pipe.as_fd(), PollFlags::POLLIN))
                    .collect();

                let n = poll(&mut poll_fds, PollTimeout::NONE)?;
                assert!(n > 0); // There should be at least one fd ready.

                poll_fds.iter().enumerate()
                    .filter(|(_, pfd)| !pfd.revents().unwrap().is_empty())
                    .map(|(i, _)| i)
                    .collect()
            };

            // Move shards to readable_shards or back to shards based on poll results.
            // We iterate in reverse order so indices remain valid as we remove elements.
            let shards = {
                let capacity = self.shards.capacity();
                std::mem::replace(&mut self.shards, Vec::with_capacity(capacity))
            };
            for (i, shard) in shards.into_iter().enumerate() {
                if readable_indices.contains(&i) {
                    self.readable_shards.push(shard);
                } else {
                    self.shards.push(shard);
                }
            }
        }

        Ok(self.readable_shards.pop())
    }

    fn process_small_file_marker(&mut self, marker: image::Marker) -> Result<()> {
        use marker::Body::*;

        assert!(marker.seq == self.small_file_seq);
        match marker.body {
            Some(Filename(filename)) => {
                if &*filename == "metadata.json" {
                    eprintln!("GOT METADATA FILENAME MARKER");
                }
                assert!(self.current_img_file.is_none());
                let img_file = self.img_store.create(&filename)?;
                self.current_img_file = Some((filename.into_boxed_str(), img_file));
            }
            Some(FileData(size)) => {
                let (filename, img_file) = self.current_img_file.as_mut()
                    .ok_or_else(|| anyhow!("Unexpected FileData marker"))?;

                if filename.as_ref() == "metadata.json" {
                    // there is never going to be more than one chunk of FileData for metadata
                    let mut metadata_bytes = Vec::with_capacity(size as usize);
                    let pipe = &mut self.small_file_shard.pipe;
                    pipe.take(size as u64).read_to_end(&mut metadata_bytes)?;
                    self.metadata = Some(serde_json::from_slice(&metadata_bytes)?);
                    eprintln!("GOT METADATA: {:#?}", self.metadata.as_ref().unwrap());
                } else {
                    img_file.write_all_from_pipe(&mut self.small_file_shard.pipe, size as usize)?;
                }
                self.small_file_shard.bytes_read += size as u64;
            }
            Some(FileEof(true)) => {
                let (filename, img_file) = self.current_img_file.take()
                    .ok_or_else(|| anyhow!("Unexpected FileEof marker"))?;

                if filename.as_ref() != "metadata.json" {
                    self.img_store.insert(filename, img_file);
                }
                self.current_img_file = None;
            }
            _ => bail!("Malformed image marker"),
        }
        Ok(())
    }

    // returns file metadata
    pub fn drain_small_file_shard(&mut self) -> Result<HashMap<Box<str>, usize>> {
        loop {
            match pb_read_next(&mut self.small_file_shard.pipe)? {
                None => {
                    break;
                }
                Some((marker, marker_size)) => {
                    self.small_file_shard.bytes_read += marker_size as u64;
                    self.process_small_file_marker(marker)?;
                    self.small_file_seq += 1;
                }
            }
        }
        assert!(self.metadata.is_some());
        // don't really like the clone
        let metadata = self.metadata.as_mut().unwrap().clone();
        // never gonna need the original now
        self.metadata = None;
        Ok(metadata)
    }

    /// Returns successfully when the image has been fully deserialized. This is our main loop.
    pub fn drain_all(&mut self) -> Result<()> {
        while let Some(shard) = self.get_next_readable_shard()? {
            self.drain_shard(shard)?;
        }
        ensure!(self.image_eof, "No shards to read from");
        Ok(())
    }
}

fn spawn_serve_img(
    images_dir: &Path,
    progress_pipe: fs::File,
    small_file_reciever: Receiver<(String, fs_parallel::FileContent)>,
    receiver: Receiver<(String, fs_parallel::FileContent)>,
    file_list: Vec<String>,
    semaphore: Arc<Semaphore>
) -> thread::JoinHandle<Result<()>> {
    let dir = images_dir.to_path_buf();
    thread::spawn(move || {
        serve_img(&dir, progress_pipe, small_file_reciever, receiver, file_list, semaphore)
    })
}

fn send_over_chunks(
    filename: &String,
    mut chunks: VecDeque<fs_parallel::FileContent>,
    pipe: &mut UnixPipe,
    semaphore: &Arc<Semaphore>
) -> Result<bool> {
    let mut res = false;
    while let Some(file_content) = chunks.pop_front() {
        match file_content {
            FileContent::Eof => {
                // we have sent everything
                eprintln!("BHAVIK: STREAMER: done with {filename}");
                res = true;
                break;
            }
            FileContent::Content(chunk) => {
                pipe.vmsplice_all(&chunk)?;
                semaphore.release(chunk.len() as isize);
            }
        }
    }
    Ok(res)
}

/// `serve_img()` serves the in-memory image store to Client.
fn serve_img(
    images_dir: &Path,
    mut progress_pipe: fs::File,
    small_file_reciever: Receiver<(String, fs_parallel::FileContent)>,
    receiver: Receiver<(String, fs_parallel::FileContent)>,
    file_list: Vec<String>,
    semaphore: Arc<Semaphore>
) -> Result<()>
{
    let mut store: HashMap<String, VecDeque<fs_parallel::FileContent>> = HashMap::new();

    for (filename, buf) in small_file_reciever {
        eprintln!("got buf: {filename}, {:?}", buf);
        store.entry(filename)
            .or_insert_with(VecDeque::new)
            .push_back(buf);
    }

    eprintln!("created store with small files: {:#?}", store);

    let listener = Listener::bind_for_restore(images_dir)?;
    emit_progress(&mut progress_pipe, "socket-init");

    // Setup the poller to monitor the server socket
    enum PollType {
        Listener(Listener),
        Client(Connection),
    }

    let mut poller = Poller::new()?;
    let listener_key = poller.add(listener.as_raw_fd(), PollType::Listener(listener), EpollFlags::EPOLLIN)?;

    let mut filenames_of_sent_files = HashSet::new();
    let available_files: HashSet<String> = file_list.clone().into_iter().collect();
    let mut reciever_eof = false;
    let mut open_pipes: Vec<(String, UnixPipe)> = vec![];

    let epoll_capacity = 16;
    loop {
        // get a chunk from reciever
        if !reciever_eof {
            loop {
                match receiver.try_recv() {
                    Ok((filename, buf)) => {
                        store.entry(filename)
                            .or_insert_with(VecDeque::new)
                            .push_back(buf);
                    },
                    Err(TryRecvError::Disconnected) => { reciever_eof = true; break; },
                    Err(TryRecvError::Empty) => { break; }
                }
            }
        }

        let mut idices_to_remove = vec![];
        for (i, (filename, pipe)) in open_pipes.iter_mut().enumerate() {
            match store.remove(filename.as_str()) {
                Some(chunks) => {
                    eprintln!("sending over more data to open_pipe: {filename}");
                    let sent_all = send_over_chunks(&filename, chunks, pipe, &semaphore)?;
                    if sent_all {
                        idices_to_remove.push(i);
                    }
                },
                None => {}
            }
        }

        for i in idices_to_remove.into_iter().rev() {
            eprintln!("DONE WITH FILE: {} closing pipe", open_pipes[i].0);
            open_pipes.remove(i);
        }

        let obj = poller.poll(epoll_capacity)?;
        let Some((_, poll_obj)) = obj else { break };
        match poll_obj {
            PollType::Listener(listener) => { // New connection waiting, accept it
                let conn = listener.accept()?;
                poller.add(conn.as_raw_fd(), PollType::Client(conn), EpollFlags::EPOLLIN)?;
            }
            PollType::Client(client) => {
                match client.read_next_file_request()? {
                    Some(ref filename) if filename == "stop-listener" => {
                        // Stop accepting any new connections. Pending files will still be
                        // processed.
                        poller.remove(listener_key)?;
                    }
                    // check if filename has a wildcard
                    Some(ref pattern) if pattern.contains('*') || pattern.is_empty() => {
                        // List all files in the image store.
                        client.send_file_list_reply(util::filter_files(&file_list, pattern))?;
                    }
                    Some(filename) => {
                        eprintln!("got a file request for: {}", filename);
                        if !available_files.contains(&filename) {
                            eprintln!("file does not exist: {}", filename);
                            client.send_file_reply(false, Some(FileStatus::DoesNotExist))?; // false means that the file does not exist.
                        } else {
                            match store.remove(&filename) {
                                Some(chunks) => {
                                    // we should not get anymore requests for the file
                                    filenames_of_sent_files.insert(filename.clone());
                                    client.send_file_reply(true, Some(FileStatus::Ready))?; // true means that the file exists.
                                    let mut pipe = client.recv_pipe()?;
                                    // Try setting the pipe capacity. Failing is okay.
                                    let _ = pipe.set_capacity(CLIENT_PIPE_DESIRED_CAPACITY);
                                    // send as much data as we have available right now and then
                                    // add it to the list of fds we have that are open
                                    let res = send_over_chunks(&filename, chunks, &mut pipe, &semaphore)?;
                                    if !res {
                                        eprintln!("not done with {filename} adding it to open_pipes");
                                        open_pipes.push((filename, pipe));
                                    }
                                }
                                None => {
                                    if available_files.contains(&filename) && !filenames_of_sent_files.contains(&filename) {
                                        client.send_file_reply(true, Some(FileStatus::NotReady))?;
                                    } else {
                                        // If we keep the image file in our process, Client will also
                                        // have a copy of the image file. This uses x2 the memory for an image
                                        // file. For large files like memory pages, we could very much go over
                                        // the machine memory capacity.
                                        ensure!(!filenames_of_sent_files.contains(&filename) && !available_files.contains(&filename),
                                            "Client is requesting the image file `{}` multiple times. \
                                            This is not allowed to keep the memory usage low", &filename);
                                        client.send_file_reply(false, Some(FileStatus::DoesNotExist))?; // false means that the file does not exist.
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        // Do nothing.
                    }
                }
            }
        }
    }

    Ok(())
}

fn drain_shards_into_img_store<Store: ImageStore>(
    img_store: &mut Store,
    progress_pipe: &mut fs::File,
    shard_pipes: Vec<UnixPipe>,
    ext_file_pipes: Vec<(String, UnixPipe)>,
) -> Result<()>
{
    let mut shards: Vec<Shard> = shard_pipes.into_iter().map(Shard::new).collect();

    // The content of the `ext_file_pipes` are streamed out directly, and not buffered in memory.
    // This is important to avoid blowing up our memory budget. These external files typically
    // contain a checkpointed filesystem, which is large.
    let mut overlayed_img_store = image_store::fs_overlay::Store::new(img_store);
    for (filename, mut pipe) in ext_file_pipes {
        // Despite the misleading name, the pipe is not for Client, it's most likely for `tar`, but
        // it gets to enjoy the same pipe capacity. If we fail to increase the pipe capacity,
        // it's okay. This is just for better performance.
        let _ = pipe.set_capacity(CLIENT_PIPE_DESIRED_CAPACITY);
        overlayed_img_store.add_overlay(filename, pipe);
    }

    let mut img_deserializer = ImageDeserializer::new(&mut overlayed_img_store, &mut shards);
    // don't care about metadata in this case
    let _ = img_deserializer.drain_small_file_shard()?;
    img_deserializer.drain_all()?;

    let stats = Stats {
        shards: shards.iter().map(|s| ShardStat {
            size: s.bytes_read,
            transfer_duration_millis: s.transfer_duration_millis,
        }).collect(),
    };
    emit_progress(progress_pipe, &serde_json::to_string(&stats)?);

    Ok(())
}

/// Description of the arguments can be found in main.rs
pub fn serve(images_dir: &Path,
    progress_pipe: fs::File,
    shard_pipes: Vec<UnixPipe>,
    ext_file_pipes: Vec<(String, UnixPipe)>,
    tcp_listen_remaps: Vec<(u16, u16)>,
    memory_limit: Option<usize>
) -> Result<()>
{
    create_dir_all(images_dir)?;
    let limit = match memory_limit {
        None => {
            sysinfo()?.ram_total() as isize / MB as isize
        },
        Some(limit) => limit as isize
    };
    eprintln!("using memory limit: {}", limit);
    let semaphore = Arc::new(semaphore::Semaphore::new(limit * MB as isize));
    let (sender, reciever) = mpsc::channel();
    let (small_file_sender, small_file_reciever) = mpsc::channel();

    let mut file_sender = image_store::fs_parallel::FileSender::new(sender, small_file_sender, Arc::clone(&semaphore));

    // see drain_shards_into_img_store for context
    let mut shards: Vec<Shard> = shard_pipes.into_iter().map(Shard::new).collect();
    let mut overlayed_img_store = image_store::fs_overlay::Store::new(&mut file_sender);
    for (filename, mut pipe) in ext_file_pipes {
        let _ = pipe.set_capacity(CLIENT_PIPE_DESIRED_CAPACITY);
        overlayed_img_store.add_overlay(filename, pipe);
    }

    let mut img_deserializer = ImageDeserializer::new(&mut file_sender, &mut shards);
    let metadata = img_deserializer.drain_small_file_shard()?;

    // TODO: deal with patch_img while serving
    // patch_img(&mut file_sender, tcp_listen_remaps)?;
    //

    let file_list = metadata.keys().map(|filename| filename.to_string()).collect();
    let handle = spawn_serve_img(images_dir, progress_pipe, small_file_reciever, reciever, file_list, Arc::clone(&semaphore));

    img_deserializer.drain_all()?;
    eprintln!("BHAVIK: DRAINED EVERYTHING");
    file_sender.close_sender();
    eprintln!("BHAVIK: CLOSED SENDER ONLY JOINING IS LEFT");
    let _ = handle.join();
    Ok(())
}

/// Description of the arguments can be found in main.rs
pub fn extract(images_dir: &Path,
    mut progress_pipe: fs::File,
    shard_pipes: Vec<UnixPipe>,
    ext_file_pipes: Vec<(String, UnixPipe)>,
) -> Result<()>
{
    create_dir_all(images_dir)?;

    // extract on disk
    let mut file_store = image_store::fs::Store::new(images_dir);
    drain_shards_into_img_store(&mut file_store, &mut progress_pipe, shard_pipes, ext_file_pipes)?;

    Ok(())
}
