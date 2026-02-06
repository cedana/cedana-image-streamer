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

//! CLI argument parsing module.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use structopt::{clap::AppSettings, StructOpt};

pub fn parse_ext_fd(s: &str) -> Result<(String, i32)> {
    let mut parts = s.split(':');
    Ok(match (parts.next(), parts.next(), parts.next()) {
        (Some(filename), Some(fd), None) => {
            let filename = filename.to_string();
            let fd = fd.parse().context("Provided ext fd is not an integer")?;
            (filename, fd)
        }
        _ => bail!("Format is filename:fd"),
    })
}

pub fn parse_port_remap(s: &str) -> Result<(u16, u16)> {
    let mut parts = s.split(':');
    Ok(match (parts.next(), parts.next(), parts.next()) {
        (Some(old_port), Some(new_port), None) => {
            let old_port = old_port
                .parse()
                .context("Provided old_port is not a u16 integer")?;
            let new_port = new_port
                .parse()
                .context("Provided new_port is not a u16 integer")?;
            (old_port, new_port)
        }
        _ => bail!("Format is old_port:new_port"),
    })
}

#[derive(StructOpt, PartialEq, Debug)]
#[structopt(about,
    // When showing --help, we want to keep the order of arguments defined
    // in the `Opts` struct, as opposed to the default alphabetical order.
    global_setting(AppSettings::DeriveDisplayOrder),
    // help subcommand is not useful, disable it.
    global_setting(AppSettings::DisableHelpSubcommand),
    // subcommand version is not useful, disable it.
    global_setting(AppSettings::VersionlessSubcommands),
)]
pub struct Opts {
    /// Images directory where the client UNIX socket is created during streaming operations.
    // The short option -D mimics CRIU's short option for its --images-dir argument.
    #[structopt(short = "D", long)]
    pub images_dir: PathBuf,

    /// File descriptors of shards. Multiple fds may be passed as a comma separated list.
    /// Defaults to 0 or 1 depending on the operation.
    // require_delimiter is set to avoid clap's non-standard way of accepting lists.
    #[structopt(short, long, require_delimiter = true)]
    pub shard_fds: Vec<i32>,

    /// External files to incorporate/extract in/from the image. Format is filename:fd
    /// where filename corresponds to the name of the file, fd corresponds to the pipe
    /// sending or receiving the file content. Multiple external files may be passed as
    /// a comma separated list.
    #[structopt(short, long, parse(try_from_str=parse_ext_fd), require_delimiter = true)]
    pub ext_file_fds: Vec<(String, i32)>,

    /// File descriptor where to report progress. Defaults to 2.
    // The default being 2 is a bit of a lie. We dup(STDOUT_FILENO) due to ownership issues.
    #[structopt(short, long)]
    pub progress_fd: Option<i32>,

    /// When serving the image, remap on the fly the TCP listen socket ports.
    /// Format is old_port:new_port. May only be used with the serve operation.
    /// Multiple tcp port remaps may be passed as a comma separated list.
    #[structopt(long, parse(try_from_str=parse_port_remap), require_delimiter = true)]
    pub tcp_listen_remap: Vec<(u16, u16)>,

    #[structopt(subcommand)]
    pub operation: Operation,
}

#[derive(StructOpt, PartialEq, Debug)]
pub enum Operation {
    /// Capture a client image
    Capture,

    /// Serve a captured client image to client
    Serve,

    /// Extract a captured client image to the specified images_dir
    Extract,
}
