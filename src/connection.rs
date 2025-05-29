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
    fs, os::unix::{io::{AsRawFd, RawFd}, net::{UnixListener, UnixStream}}, path::Path
};
use crate::{
    criu, unix_pipe::{UnixPipe, UnixPipeImpl}, util::{pb_read_next, pb_write, recv_fd}
};
use anyhow::{Result, Context};

const IMG_STREAMER_CAPTURE_SOCKET_NAME: &str = "streamer-capture.sock";
const IMG_STREAMER_SERVE_SOCKET_NAME: &str = "streamer-serve.sock";

/// The role of the `Listener` and `Connection` is to handle communication over
/// the image socket.

pub struct Listener {
    listener: UnixListener,
}

impl Listener {
    fn bind(socket_path: &Path) -> Result<Self> {
        // 1) We unlink the socket path to avoid EADDRINUSE on bind() if it already exists.
        // 2) We ignore the unlink error because we are most likely getting a -ENOENT.
        //    It is safe to do so as correctness is not impacted by unlink() failing.
        let _ = fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("Failed to bind socket to {}", socket_path.display()))?;

        Ok(Self { listener })
    }

    pub fn bind_for_capture(images_dir: &Path) -> Result<Self> {
        Self::bind(&images_dir.join(IMG_STREAMER_CAPTURE_SOCKET_NAME))
    }

    pub fn bind_for_restore(images_dir: &Path) -> Result<Self> {
        Self::bind(&images_dir.join(IMG_STREAMER_SERVE_SOCKET_NAME))
    }

    // into_accept() drops the listener. If there is no need for having multiple connections,
    // use this, as it will drop the connection, otherwise use accept().
    pub fn into_accept(self) -> Result<Connection> {
        let (socket, _) = self.listener.accept()?;
        Ok(Connection { socket })
    }

    // if accepting multiple connections
    pub fn accept(&self) -> Result<Connection> {
        // Accept a new connection without consuming the listener
        let (socket, _) = self.listener.accept()?;
        Ok(Connection { socket })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }
}

pub struct Connection {
    socket: UnixStream,
}

impl Connection {
    /// Read and return the next file request. If reached EOF, returns Ok(None).
    pub fn read_next_file_request(&mut self) -> Result<Option<String>> {
        Ok(pb_read_next(&mut self.socket)?
            .map(|(req, _): (criu::ImgStreamerRequestEntry, _)| req.filename))
    }

    /// Returns the data pipe that is used to transfer the file.
    pub fn recv_pipe(&mut self) -> Result<UnixPipe> {
        UnixPipe::new(recv_fd(&mut self.socket)?)
    }

    /// During restore, client requests image files that may or may not exist.
    /// We must let client know if we hold has the requested file in question.
    /// It is done via `send_file_reply()`. Not used during checkpointing.
    pub fn send_file_reply(&mut self, exists: bool) -> Result<()> {
        pb_write(&mut self.socket, &criu::ImgStreamerReplyEntry { exists })?;
        Ok(())
    }

    pub fn send_file_list_reply(&mut self, files: Vec<String>) -> Result<()> {
        pb_write(&mut self.socket, &criu::ImgStreamerListReplyEntry { filenames: files })?;
        Ok(())
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}
