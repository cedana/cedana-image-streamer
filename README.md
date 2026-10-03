Cedana Image Streamer
====================

_cedana-image-streamer_ enables streaming of images to and from
[Cedana](https://github.com/cedana/cedana) during checkpoint/restore with low overhead.

This is a maintained fork of https://github.com/checkpoint-restore/criu-image-streamer. 

Usage
-----

**Note**: _cedana-image-streamer_ requires [this fork](https://github.com/cedana/criu) of CRIU.

To build the _cedana-image-streamer_ executable:
```sh
make
```

For installation see [locally built plugins](https://docs.cedana.ai/daemon/get-started/plugins#locally-built-plugins). For usage, check out [checkpoint/restore streaming](https://docs.cedana.ai/daemon/guides/cr-4).

During capture, each image request on `streamer-capture.sock` is followed by a file
descriptor. Filenames starting with `gpu-` must supply a fully populated memfd;
the streamer reads its contents from offset zero. Other filenames supply the read
end of a pipe. Both inputs use the same shard format for extraction and restore.

During `serve`, buffered image files are stored in memfds. After the usual
"file exists" reply on `streamer-serve.sock`, a `gpu-` request receives the fully
populated memfd directly via `SCM_RIGHTS`, positioned at offset zero. The client
does not send a pipe for these requests, and may request the same `gpu-` file more
than once (GPU dedup restores map peer workers' files); the memfd is shared, not
copied. Other filenames retain the pipe-based restore protocol and may be requested
only once.

License
-------
cedana-image-streamer is licensed under the [Apache 2.0 license](https://www.apache.org/licenses/LICENSE-2.0).

criu-image-streamer is originally licensed under the
[Apache 2.0 license](https://www.apache.org/licenses/LICENSE-2.0).
