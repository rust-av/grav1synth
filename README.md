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

### `grav1synth apply my_encode.mkv -o grainy_encode.mkv -g grain_file.txt`

Reads `my_encode.mkv`, adds film grain to it based on `grain_file.txt`, and outputs the video to `grainy_encode.mkv`

### `grav1synth generate my_encode.mkv -o grainy_encode.mkv --iso 400 --chroma`

Reads `my_encode.mkv`, adds photon-noise-based film grain to it based on the strength provided by `--iso` (up to `6400`), and outputs the video to `grainy_encode.mkv`. By default applies grain to only the luma plane. `--chroma` enables grain on chroma planes as well.

### `grav1synth remove my_encode.mkv -o clean_encode.mkv`

Reads `my_encode.mkv`, removes all synthesized film grain, and outputs the video at `clean_encode.mkv`

## Known Issues

- Currently fails to parse some video files. It is suspected that this happens on mkv files which have attachments.
