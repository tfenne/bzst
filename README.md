# bzst

**Block-compressed Zstandard**: a parallel, seekable, `zstd`-compatible container format. Said aloud, *beast*.

bzst stores a stream as a sequence of independently compressed blocks, each an ordinary Zstandard frame preceded by a small skippable frame giving its sizes, and ends with an embedded index that maps uncompressed offsets to compressed locations. Compression and decompression are therefore trivially parallel, even on a pipe, and a reader can jump to any uncompressed offset without scanning the file. A bzst file in the baseline profile is also a valid Zstandard stream: `zstd -d` reproduces the original bytes.

bzst grew out of genomics, as a successor to the BGZF layer beneath BAM, VCF and tabix-indexed files, but the format itself is domain-agnostic: it knows nothing about records, coordinates or samples. Derived formats (a BAM or VCF built on bzst, say) carry their own metadata and indices in their own skippable frames, layered on top.

## Status

The specification is a **working draft**. Field layouts and the provisional magic numbers can still change, and a file written against one revision of the draft may not be readable by the next. The Rust reference implementation tracks the current draft. There are no releases yet; design discussion happens in issues and pull requests.

## Repository layout

| Path | Contents |
|---|---|
| [`spec/`](spec) | The format specification: [`bzst.typ`](spec/bzst.typ) (Typst source) and [`bzst.pdf`](spec/bzst.pdf) (rendered) |
| [`rust/`](rust) | The Rust reference implementation: the `bzst` library crate and the `bzst` command-line tool (see [`rust/README.md`](rust/README.md)) |

## Quick start

Build the command-line tool from source. You need a Rust toolchain from [rustup](https://rustup.rs); the pinned version installs itself on first use.

```sh
cd rust
cargo build --release
target/release/bzst -k data.txt           # -> data.txt.bzst, keeping data.txt
target/release/bzst -d -c data.txt.bzst   # decompress to stdout
zstd -d -c data.txt.bzst                  # stock zstd decodes it too
```

[`rust/README.md`](rust/README.md) covers the tool's options and the library API.

## Contributing

Contributions and design feedback are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers the checks to run and how changes to the spec and the implementation fit together.

## Acknowledgements

bzst builds on the Zstandard format and its `pzstd` and `seekable_format` tools, on James Bonfield's BGZF2 proposal, and on the `zeekstd` seekable format. The specification's acknowledgements section gives full credit.

## License

- The specification (`spec/`) is licensed under [CC BY 4.0](spec/LICENSE).
- The Rust implementation (`rust/`) is licensed under the [MIT License](rust/LICENSE).

Copyright (c) 2026 Tim Fennell.
