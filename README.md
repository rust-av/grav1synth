# Grav1synth

[![docs.rs](https://img.shields.io/docsrs/grav1synth?style=for-the-badge)](https://docs.rs/grav1synth)
[![Crates.io](https://img.shields.io/crates/v/grav1synth?style=for-the-badge)](https://crates.io/crates/grav1synth)
[![LICENSE](https://img.shields.io/crates/l/grav1synth?style=for-the-badge)](https://github.com/rust-av/grav1synth/blob/main/LICENSE)

Grain Synth analyzer and editor for AV1 files

## Disclaimer etc.

This was a quick fork created to fix a dependency issue with the library so a friend could build it correctly

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

### `grav1synth diff my_source.mkv denoised_source.mkv -o grain_file.txt`

Compares `my_source.mkv` and `denoised_source.mkv` and generates a film grain table at `grain_file.txt` based on the difference between them. This will provide the most accurate estimation of source film grain.

<!-- ### `grav1synth estimate my_source.mkv -o grain_file.txt`

Analyzes `my_source.mkv` and estimates the amount of noise in the source, then generates an appropriate film grain table at `grain_file.txt`. This is less accurate than the diff method, but is significantly faster. -->

## Known Issues

- There have been reports that certain videos will fail to apply film grain properly. This is likely related to aomenc's `--keyframe-filtering=2`.
