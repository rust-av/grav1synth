use std::{path::Path, process::Command};

use anyhow::Result;

pub fn get_frame_count(video: &Path) -> Result<usize> {
    // Would it be better to use the ffmpeg API for this? Yes.
    // But it would also be an outrageous pain in the rear,
    // when I can use the command line by copy and pasting
    // one command from StackOverflow.
    let result = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-count_packets")
        .arg("-show_entries")
        .arg("stream=nb_read_packets")
        .arg("-of")
        .arg("csv=p=0")
        .arg(video)
        .output()?;
    let stdout = String::from_utf8_lossy(&result.stdout);
    Ok(stdout.trim().parse()?)
}
