use anyhow::Result;
use aws_sdk_s3::{
    Client,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use image_streamer::{
    capture::capture,
    endpoint_connection::EndpointListener,
    extract::serve,
    unix_pipe::UnixPipe,
    util::{self, create_dir_all},
    prnt,
};
use libc::dup;
use lz4_flex::frame::{FrameDecoder, FrameEncoder};
use nix::unistd::{close, pipe};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::unix::io::{FromRawFd, RawFd},
    path::{Path, PathBuf},
    thread,
    // time::Duration,
};
use std::{
    io::Cursor,
    sync::mpsc
};
use structopt::{clap::AppSettings, StructOpt};
use tokio::runtime::Runtime;

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
struct Opts {
    /// Bucket name in AWS S3 where lz4 images will be uploaded to / downloaded from.
    #[structopt(short = "B", long)]
    bucket: Option<String>,
    /// Images directory where the CRIU UNIX socket is created during streaming operations.
    // The short option -D mimics CRIU's short option for its --dir argument.
    #[structopt(short = "D", long)]
    dir: PathBuf,
    #[structopt(subcommand)]
    operation: Operation,
    #[structopt(short, long)]
    num_pipes: usize,
}

#[derive(StructOpt, PartialEq, Debug)]
enum Operation {
    /// Capture a CRIU image
    Capture,

    /// Serve a captured CRIU image to CRIU
    Serve,
}

fn spawn_capture_handles_local(
    dir_path: PathBuf,
    num_pipes: usize,
    r_fds: Vec<RawFd>,
) -> Vec<thread::JoinHandle<()>> {
    (0..num_pipes)
        .map(|i| {
            let r_fd = r_fds[i].clone();
            let path = dir_path.clone();
            thread::spawn(move || {
                let output_file_path = path.join(&format!("img-{}.lz4", i));
                let output_file = File::create(&output_file_path)
                                    .expect("Unable to create output file path");
                let mut encoder = FrameEncoder::new(output_file);
                let mut input_file = unsafe { File::from_raw_fd(r_fd) };
                let mut buffer = [0; 1048576];
                loop {
                    match input_file.read(&mut buffer) {
                        Ok(0) => {
                            let _ = close(r_fd);
                            let _ = encoder.finish();
                            return;
                        }
                        Ok(bytes_read) => {
                            encoder.write_all(&buffer[..bytes_read])
                                            .expect("Unable to write all bytes");
                            encoder.flush().expect("Failed to flush encoder");
                        },
                        Err(e) => {
                            if e.kind() != io::ErrorKind::Interrupted {
                                prnt!("err={}",e);
                                return;
                            }
                        }
                    }
                }
            })
        })
        .collect()
}

fn create_s3_client() -> Client {
    let config = Runtime::new()
        .unwrap()
        .block_on(aws_config::load_from_env());
    Client::new(&config)
}

async fn initiate_multipart_upload(client: &Client, bucket: &str, key: &str) -> String {
    client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("Failed to initiate multipart upload")
        .upload_id
        .expect("Upload ID not provided")
}

async fn upload_part(
    client: &Client,
    bucket: &str,
    key: &str,
    upload_id: &str,
    part_number: i32,
    data: Vec<u8>,
) -> CompletedPart {
    let part = client
        .upload_part()
        .buc    ket(bucket)
        .key(key)
        .upload_id(upload_id)
        .part_number(part_number)
        .body(ByteStream::from(data))
        .send()
        .await
        .expect("Failed to upload part");

    CompletedPart::builder()
        .part_number(part_number)
        .e_tag(part.e_tag().unwrap().to_string())
        .build()
}

async fn complete_multipart_upload(
    client: &Client,
    bucket: &str,
    key: &str,
    upload_id: &str,
    completed_parts: Vec<CompletedPart>,
) {
    let completed_mpu = CompletedMultipartUpload::builder()
        .set_parts(Some(completed_parts))
        .build();

    client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .multipart_upload(completed_mpu)
        .send()
        .await
        .expect("Failed to complete multipart upload");
}

fn spawn_capture_handles_remote(
    bucket: String,
    num_pipes: usize,
    r_fds: Vec<RawFd>,
) -> Vec<thread::JoinHandle<()>> {
    prnt!("entered spawn_capture_handles_remote");
    (0..num_pipes)
        .map(|i| {
            let bucket_name = bucket.clone();
            let r_fd = r_fds[i].clone();
            let client = create_s3_client();
            thread::spawn(move || {
                let key = format!("img-{}.lz4", i);
                prnt!("key = {}",key);
                let runtime = Runtime::new().expect("Failed to create Tokio runtime");
                let upload_id = runtime.block_on(async {
                    initiate_multipart_upload(&client, &bucket_name, &key).await
                });
                let mut completed_parts = Vec::new();
                let mut total_bytes_read: u64 = 0;
                let mut part_number = 1;
                let mut encoder = FrameEncoder::new(Vec::new());
                let mut input_file = unsafe { File::from_raw_fd(r_fd) };
                let mut buffer = vec![0; 5242880];
                loop {
                    match input_file.read(&mut buffer) {
                        Ok(0) => {
                            prnt!("thread {} read {} bytes and total = {}", i.clone(), 0, total_bytes_read);
                            let _ = close(r_fd);
                            prnt!("encoder.get_ref().len() before last drain [{}]",encoder.get_ref().len());
                            encoder.flush().expect("Failed to flush encoder");
                            prnt!("encoder.get_ref().len() after flush [{}]",encoder.get_ref().len());
                            let compressed_data = encoder.get_mut().drain(..).collect();
                            prnt!("encoder.get_ref().len() after last drain [{}]",encoder.get_ref().len());
                            if let Ok(_) = encoder.finish() {
                                prnt!("encoder.finish returned ok");
                                let completed_part = runtime.block_on(async {
                                    upload_part(&client, &bucket_name, &key, &upload_id, part_number, compressed_data).await
                                });
                                completed_parts.push(completed_part);
                            } else {
                                prnt!("encoder.finish returned err");
                            }
                            runtime.block_on(async {
                                complete_multipart_upload(&client, &bucket_name, &key, &upload_id, completed_parts.clone()).await;
                            });
                            prnt!("total_bytes_read = {}", total_bytes_read);
                            return;
                        }
                        Ok(bytes_read) => {
                            total_bytes_read += bytes_read as u64;
                            prnt!("thread {} read {} bytes and total = {}", i.clone(), bytes_read, total_bytes_read);
                            encoder.write_all(&buffer[..bytes_read]).expect("Unable to write bytes");
                            encoder.flush().expect("Failed to flush encoder");
                            prnt!("encoder.get_ref().len() [{}] vs. 1048576", encoder.get_ref().len());
                            if encoder.get_ref().len() >= 5242880 { // minimum allowed size for S3
                                let compressed_data = encoder.get_mut().drain(..).collect();
                                prnt!("encoder.get_ref().len() after drain [{}]",encoder.get_ref().len());
                                let completed_part = runtime.block_on(async {
                                    upload_part(&client, &bucket_name, &key, &upload_id, part_number, compressed_data).await
                                });
                                completed_parts.push(completed_part);
                                part_number += 1;
                            }
                        }
                        Err(e) => {
                            if e.kind() != std::io::ErrorKind::Interrupted {
                                eprintln!("Error reading from fd {}: {}", r_fd, e);
                                return;
                            }
                        }
                    }
                }
            })
        })
        .collect()
}

fn spawn_serve_handles_local(
    dir_path: PathBuf,
    num_pipes: usize,
    w_fds: Vec<RawFd>
) -> Vec<thread::JoinHandle<()>> {
    prnt!("entered spawn_serve_handles_local");
    (0..num_pipes)
        .map(|i| {
            let w_fd = w_fds[i].clone();
            let path = dir_path.clone();
            let (tx, rx) = mpsc::channel();
            let handle = thread::spawn(move || {
                let input_file_path = path.join(&format!("img-{}.lz4", i));
                let input_file = File::open(&input_file_path)
                                        .expect("Unable to open input file path");
                let mut output_file = unsafe { File::from_raw_fd(w_fd) };
                let mut decoder = FrameDecoder::new(input_file);
                let mut buffer = vec![0; 1048576];
                tx.send(format!("thread {} ready to read bytes", i.clone())).unwrap();
                
                loop {
                    match decoder.read(&mut buffer) {
                        Ok(0) => {
                            return;
                        }
                        Ok(bytes_read) => {
                            let _ = output_file.write_all(&buffer[..bytes_read])
                                        .expect("could not write all bytes");
                            // thread::sleep(Duration::from_millis(100));
                        },
                        Err(e) => {
                            if e.kind() != io::ErrorKind::Interrupted {
                                prnt!("err={}",e);
                                return;
                            }
                        }
                    }
                }
                // })
            });
            match rx.recv() {
                Ok(message) => { prnt!("Received message: {}", message); },
                Err(e) => { prnt!("Failed to receive message: {}", e); },
            };
            handle
        })
        .collect()
}

fn spawn_serve_handles_remote( // should be finishing earlier
    bucket: String,
    num_pipes: usize,
    w_fds: Vec<RawFd>,
) -> Vec<thread::JoinHandle<()>> {
    prnt!("entered spawn_serve_handles_remote");
    (0..num_pipes)
        .map(|i| {
            prnt!("entered i = {}",i.clone());
            let bucket_name = bucket.clone();
            let w_fd = w_fds[i].clone();
            let key = format!("img-{}.lz4", i);
            prnt!("starting to spawn serve handle for {}", &key);
            let client = create_s3_client();
            prnt!("{} returned from create_s3_client", &key);
            let (tx, rx) = mpsc::channel();
            let handle = thread::spawn(move || {
                let mut total_bytes_written = 0;
                let mut output_file = unsafe { File::from_raw_fd(w_fd) };
                let mut buffer = vec![0; 1048576];
                prnt!("finished setup");
                let runtime = Runtime::new().expect("Failed to create Tokio runtime");
                prnt!("created runtime");
                runtime.block_on(async {
                    prnt!("entered runtime");
                    let body = client
                                .get_object()
                                .bucket(&bucket_name)
                                .key(&key)
                                .send()
                                .await
                                .expect("can't find key")
                                .body
                                .collect()
                                .await
                                .expect("can't find body");
                    prnt!("passed part_body_result");
                    // memory issue due to holding this all in memory?
                    let bytes = body.into_bytes();
                    let cursor = Cursor::new(bytes);
                    let mut decoder = FrameDecoder::new(cursor);
                    tx.send(format!("thread {} ready to read", i.clone())).unwrap();
                    loop {
                        match decoder.read(&mut buffer) {
                            Ok(0) => {
                                prnt!("{}: decoder read {} bytes (total = {})", &key, 0, total_bytes_written);
                                // drop(decoder);
                                return
                            },
                            Ok(bytes_read) => {
                                total_bytes_written += bytes_read;
                                prnt!("{}: decoder read {} bytes (total = {})", &key, bytes_read, total_bytes_written);
                                let _ = output_file.write_all(&buffer[..bytes_read])
                                            .expect("could not write all bytes");
                            },
                            Err(e) => {
                                if e.kind() != io::ErrorKind::Interrupted {
                                    prnt!("err={}",e);
                                    return;
                                }
                            }
                        }
                    }
                });
            });
            match rx.recv() {
                Ok(message) => { prnt!("Received message: {}", message); },
                Err(e) => { prnt!("Failed to receive message: {}", e); },
            };
            handle
        })
        .collect()
}

fn join_handles(handles: Vec<thread::JoinHandle<()>>) {
    for handle in handles {
        match handle.join() {
            Ok(_) => { prnt!("Shard thread completed successfully"); },
            Err(e) => { prnt!("Shard thread panicked: {:?}", e); },
        }
    }
}

fn do_capture(dir: &Path, num_pipes: usize, bucket: Option<String>) -> Result<()> {
    let mut shard_pipes: Vec<UnixPipe> = Vec::new();
    let mut r_fds: Vec<RawFd> = Vec::new();
    for _ in 0..num_pipes {
        let (r_fd, w_fd): (RawFd, RawFd) = pipe()?;
        let dup_fd = unsafe { dup(w_fd) };
        close(w_fd).expect("Failed to close original write file descriptor");

        r_fds.push(r_fd);
        shard_pipes.push(unsafe { File::from_raw_fd(dup_fd) });
    }

    let _ret = create_dir_all(dir);

    let gpu_listener = EndpointListener::bind(dir, "gpu-capture.sock")?;
    let criu_listener = EndpointListener::bind(dir, "streamer-capture.sock")?;
    let ced_listener = EndpointListener::bind(dir, "ced-capture.sock")?;
    eprintln!("r");

    let handle = thread::spawn(move || {
        let _res = capture(shard_pipes, gpu_listener, criu_listener, ced_listener);
    });
    // thread::sleep(Duration::from_millis(1000));

    let handles = match bucket {
        Some(s) => spawn_capture_handles_remote(s, num_pipes, r_fds),
        None => spawn_capture_handles_local(dir.to_path_buf(), num_pipes, r_fds),
    };

    match handle.join() {
        Ok(_) => { prnt!("Capture thread completed successfully"); },
        Err(e) => { prnt!("Capture thread panicked: {:?}", e); },
    }
    join_handles(handles);

    Ok(())
}

fn do_serve(dir: &Path, num_pipes: usize, bucket: Option<String>) -> Result<()> {
    prnt!("entered do_serve");
    let mut shard_pipes: Vec<UnixPipe> = Vec::new();
    let mut w_fds: Vec<RawFd> = Vec::new();
    for _ in 0..num_pipes {
        let (r_fd, w_fd): (RawFd, RawFd) = pipe()?;
        let dup_fd = unsafe { dup(r_fd) };
        close(r_fd).expect("Failed to close original read file descriptor");

        w_fds.push(w_fd);
        shard_pipes.push(unsafe { File::from_raw_fd(dup_fd) });
    }
    
    let ced_listener = EndpointListener::bind(dir, "ced-serve.sock")?;
    let gpu_listener = EndpointListener::bind(dir, "gpu-serve.sock")?;
    let criu_listener = EndpointListener::bind(dir, "streamer-serve.sock")?;
    let ready_path = dir.join("ready");

    let handle = thread::spawn(move || { // should be taking longer
        let _res = serve(shard_pipes, &ready_path, ced_listener, gpu_listener, criu_listener);
    });
    // thread::sleep(Duration::from_millis(1000));

    let handles = match bucket {
        Some(ref s) => { 
            prnt!("remoting");
            spawn_serve_handles_remote(s.to_string(), num_pipes, w_fds.clone())
        },
        None => {
            prnt!("not remoting");
            spawn_serve_handles_local(dir.to_path_buf(), num_pipes, w_fds.clone())
        },
    };
    prnt!("finished making handles");
    
    join_handles(handles); // taking much much longer in remote case
    // eprintln!("r");

    match handle.join() {
        Ok(_) => prnt!("Serve thread completed successfully"),
        Err(e) => prnt!("Serve thread panicked: {:?}", e),
    }

    // for w in w_fds {
    //     close(w).expect("failed to close w_fd");
    // }

    Ok(())
}

fn do_main() -> Result<()> {
    let opts: Opts = Opts::from_args();

    let bucket = opts.bucket;
    let dir = Path::new(&opts.dir);
    let num_pipes = opts.num_pipes;

    match opts.operation {
        Operation::Capture => do_capture(dir, num_pipes, bucket),
        Operation::Serve => do_serve(dir, num_pipes, bucket),
    }?;
    Ok(())
}

fn main() {
    if let Err(e) = do_main() {
        eprintln!("cedana-image-streamer Error: {:#}", e);
    }
}
