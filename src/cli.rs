use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use crate::parser::{decode_packet, BitstreamParser};

#[derive(Parser, Debug)]
pub struct Args {
    pub input: PathBuf,
}

pub fn run(args: &Args) -> Result<()> {
    let mut parser = BitstreamParser::open(&args.input)?;

    // TODO: Support running through all frames.
    // For the very first iteration, we only check the first frame for grain.
    let packet = parser.read_packet()?.expect("Video has no packets");
    let decoded_packet = decode_packet(&packet)?;

    Ok(())
}
