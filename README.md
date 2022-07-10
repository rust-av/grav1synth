# Grav1synth

Grain Synth analyzer and editor for AV1 files

## Build

- Prerequisites:
  - ffmpeg headers
  - Rust compiler
- Pull the repo
- Run `cargo build --release`
- Copy the binary from `target/release/grav1synth` to wherever you want

## Usage

### `grav1synth inspect my_encode.mkv -o grain_file.txt`

Reads `my_encode.mkv` and outputs a film grain table file at `grain_file.txt`

### More commands coming soon
