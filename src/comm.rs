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
    io::{self, BufWriter, Read, Write, Seek, SeekFrom},
    os::unix::io::{FromRawFd, RawFd},
    path::{Path, PathBuf},
    thread,
    time::Duration,
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
                let mut output_file = File::create(&output_file_path)
                                    .expect("Unable to create output file path");
                let mut total_bytes_read: u64 = 0;
                let size_placeholder = [0u8; 8];
                output_file.write_all(&size_placeholder)
                    .expect("Could not write size placeholder");
                let mut buf_writer = BufWriter::new(output_file);
                let mut encoder = FrameEncoder::new(&mut buf_writer);
                let mut input_file = unsafe { File::from_raw_fd(r_fd) };
                let mut buffer = [0; 1048576];
                loop {
                    match input_file.read(&mut buffer) {
                        Ok(0) => {
                            let _ = close(r_fd);
                            let _ = encoder.finish();
                            // prepend uncompressed size
                            let mut output_file = buf_writer.into_inner()
                                .expect("Could not get file back from buf writer");
                            output_file.seek(SeekFrom::Start(0))
                                .expect("Could not seek to start");
                            output_file.write_all(&total_bytes_read.to_le_bytes())
                                .expect("Could not write total uncompressed size");
                            output_file.flush().expect("Failed to flush BufWriter");

                            return;
                        }
                        Ok(bytes_read) => {
                            total_bytes_read += bytes_read as u64;
                            encoder.write_all(&buffer[..bytes_read])
                                            .expect("Unable to write all bytes");
                            encoder.flush().expect("Failed to flush encoder");
                        },
                        Err(e) => {
                            if e.kind() != io::ErrorKind::Interrupted {
                                prnt!(&format!("err={}",e));
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
        .bucket(bucket)
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
                prnt!(&format!("key = {}",key));
                let runtime = Runtime::new().expect("Failed to create Tokio runtime");
                let upload_id = runtime.block_on(async {
                    initiate_multipart_upload(&client, &bucket_name, &key).await
                });
                let mut completed_parts = Vec::new();
                let mut total_bytes_read: u64 = 0;
                let mut part_number = 1;
                let mut encoder = FrameEncoder::new(Vec::new());
                let mut input_file = unsafe { File::from_raw_fd(r_fd) };
                let mut buffer = [0; 1048576];
                loop {
                    match input_file.read(&mut buffer) {
                        Ok(0) => {
                            let _ = close(r_fd);
                            if let Ok(compressed_data) = encoder.finish() {
                                let completed_part = runtime.block_on(async {
                                    upload_part(&client, &bucket_name, &key, &upload_id, part_number, compressed_data).await
                                });
                                completed_parts.push(completed_part);
                            }
                            runtime.block_on(async {
                                complete_multipart_upload(&client, &bucket_name, &key, &upload_id, completed_parts.clone()).await;
                            });
                            eprintln!("total_bytes_read = {}", total_bytes_read);
                            let _ = close(r_fd);
                            return;
                        }
                        Ok(bytes_read) => {
                            total_bytes_read += bytes_read as u64;
                            println!("thread {} read {} bytes and total = {}", i.clone(), bytes_read, total_bytes_read);
                            encoder.write_all(&buffer[..bytes_read]).expect("Unable to write bytes");
                            if encoder.get_ref().len() > 5 * 1024 * 1024 { // 5MB minimum for S3?
                                let compressed_data = encoder.get_mut().drain(..).collect();
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
                let mut input_file = File::open(&input_file_path)
                                        .expect("Unable to open input file path");
                // get prepended uncompressed size
                let mut size_buf = [0u8; 8];
                input_file.read_exact(&mut size_buf).expect("Could not read decompressed size");
                let bytes_to_read = u64::from_le_bytes(size_buf);
                let mut output_file = unsafe { File::from_raw_fd(w_fd) };
                let mut decoder = FrameDecoder::new(input_file);
                let mut buffer = [0; 1048576];
                let mut total_bytes_read = 0;
                tx.send(format!("thread {} ready to read {} bytes", i.clone(), bytes_to_read))
                        .unwrap();
                
                loop {
                    match decoder.read(&mut buffer) {
                        Ok(0) => {
                            if total_bytes_read == bytes_to_read {
                                return;
                            }
                        }
                        Ok(bytes_read) => {
                            total_bytes_read += bytes_read as u64;
                            let _ = output_file.write_all(&buffer[..bytes_read])
                                        .expect("could not write all bytes");
                        },
                        Err(e) => {
                            if e.kind() != io::ErrorKind::Interrupted {
                                prnt!(&format!("err={}",e));
                                return;
                            }
                        }
                    }
                }
            });
            match rx.recv() {
                Ok(message) => prnt!(message),
                Err(e) => prnt!(&format!("Failed to receive message: {}", e)),
            };
            handle
        })
        .collect()
}

fn spawn_serve_handles_remote(
    bucket: String,
    num_pipes: usize,
    w_fds: Vec<RawFd>,
) -> Vec<thread::JoinHandle<()>> {
    println!("entered spawn_serve_handles_remote");
    (0..num_pipes)
        .map(|i| {
            println!("entered i = {}",i.clone());
            let bucket_name = bucket.clone();
            let w_fd = w_fds[i].clone();
            let key = format!("img-{}.lz4", i);
            println!("starting to spawn serve handle for {}", &key);
            let client = create_s3_client();
            println!("{} returned from create_s3_client", &key);
            thread::spawn(move || {
                let mut total_bytes_written = 0;
                let mut output_file = unsafe { File::from_raw_fd(w_fd) };
                let mut buffer = [0; 1048576];
                println!("finished setup");
                let rt = tokio::runtime::Runtime::new().unwrap();
                let body = rt.block_on(async {client
                                        .get_object()
                                        .bucket(&bucket_name)
                                        .key(&key)
                                        .send()
                                        .await
                                        .expect("can't find key")
                                        .body
                                        .collect()
                                        .await
                                        .expect("can't find body")});
                println!("passed part_body_result");
                let bytes = body.into_bytes();
                let cursor = Cursor::new(bytes);
                let mut decoder = FrameDecoder::new(cursor);
                loop {
                    match decoder.read(&mut buffer) {
                        Ok(0) => {
                            println!("{}: decoder read {} bytes (total = {})", &key, 0, total_bytes_written);
                            return
                        },
                        Ok(bytes_read) => {
                            total_bytes_written += bytes_read;
                            println!("{}: decoder read {} bytes (total = {})", &key, bytes_read, total_bytes_written);
                            let _ = output_file.write_all(&buffer[..bytes_read])
                                        .expect("could not write all bytes");
                        },
                        Err(e) => {
                            if e.kind() != io::ErrorKind::Interrupted {
                                prnt!(&format!("err={}",e));
                                return;
                            }
                        }
                    }
                }
            })
        })
        .collect()
}

fn join_handles(handles: Vec<thread::JoinHandle<()>>) {
    for handle in handles {
        match handle.join() {
            Ok(_) => prnt!("Shard thread completed successfully"),
            Err(e) => prnt!(&format!("Shard thread panicked: {:?}", e)),
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
    thread::sleep(Duration::from_millis(10));

    let handles = match bucket {
        Some(s) => spawn_capture_handles_remote(s, num_pipes, r_fds),
        None => spawn_capture_handles_local(dir.to_path_buf(), num_pipes, r_fds),
    };

    match handle.join() {
        Ok(_) => prnt!("Capture thread completed successfully"),
        Err(e) => prnt!(&format!("Capture thread panicked: {:?}", e)),
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
        close(r_fd).expect("Failed to close original write file descriptor");

        w_fds.push(w_fd);
        shard_pipes.push(unsafe { File::from_raw_fd(dup_fd) });
    }
    
    let ced_listener = EndpointListener::bind(dir, "ced-serve.sock")?;
    let gpu_listener = EndpointListener::bind(dir, "gpu-serve.sock")?;
    let criu_listener = EndpointListener::bind(dir, "streamer-serve.sock")?;
    let ready_path = dir.join("ready");
    eprintln!("r");

    let handle = thread::spawn(move || {
        let _res = serve(shard_pipes, &ready_path, ced_listener, gpu_listener, criu_listener);
    });

    thread::sleep(Duration::from_millis(10));

    let handles = match bucket {
        Some(ref s) => { 
            println!("remoting");
            spawn_serve_handles_remote(s.to_string(), num_pipes, w_fds)
        },
        None => {
            println!("not remoting");
            spawn_serve_handles_local(dir.to_path_buf(), num_pipes, w_fds)
        },
    };
    println!("finished making handles");
    
    match handle.join() {
        Ok(_) => prnt!("Serve thread completed successfully"),
        Err(e) => prnt!(&format!("Serve thread panicked: {:?}", e)),
    }
    join_handles(handles);

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
