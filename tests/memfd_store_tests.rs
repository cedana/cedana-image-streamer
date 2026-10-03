// SPDX-License-Identifier: Apache-2.0

use std::{fs::File, io::{Read, Write}, os::fd::AsFd, thread};
use anyhow::Result;
use nix::{fcntl::{fcntl, FcntlArg}, sys::stat::fstat, unistd::pipe};
use cedana_image_streamer::{
    image_store::{ImageStore, memfd},
    util::{MB, PAGE_SIZE},
};

#[test]
fn ordinary_file_is_memfd_and_releases_pages_during_pipe_transfer() -> Result<()> {
    let mut store = memfd::Store::default();
    let mut file = store.create("ordinary.img")?;
    fcntl(&file, FcntlArg::F_GET_SEALS)?;
    let data: Vec<u8> = (0..2*MB + 37).map(|i| (i % 251) as u8).collect();
    file.write_all(&data)?;
    store.insert("ordinary.img", file);
    let file = store.remove("ordinary.img").unwrap();
    let observer = file.try_clone()?;
    let blocks_before = fstat(observer.as_fd())?.st_blocks;

    let (pipe_r, pipe_w) = pipe()?;
    let mut pipe_r = File::from(pipe_r);
    let mut pipe_w = File::from(pipe_w);
    let reader = thread::spawn(move || -> Result<Vec<u8>> {
        let mut received = Vec::new();
        pipe_r.read_to_end(&mut received)?;
        Ok(received)
    });

    memfd::drain(file, &mut pipe_w)?;
    drop(pipe_w);
    assert_eq!(reader.join().unwrap()?, data);
    let blocks_after = fstat(observer.as_fd())?.st_blocks;
    assert!(blocks_after < blocks_before, "Transferred pages were not released");
    assert!(blocks_after <= (*PAGE_SIZE / 512) as i64, "More than the final partial page remains");
    Ok(())
}
