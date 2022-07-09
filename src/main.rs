#![deny(clippy::all)]
#![warn(clippy::nursery)]
#![warn(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::inconsistent_struct_constructor)]
#![allow(clippy::inline_always)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::similar_names)]
#![allow(clippy::struct_excessive_bools)]
#![allow(clippy::use_self)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::create_dir)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::default_numeric_fallback)]
#![warn(clippy::exit)]
#![warn(clippy::filetype_is_file)]
#![warn(clippy::float_cmp_const)]
#![warn(clippy::if_then_some_else_none)]
#![warn(clippy::lossy_float_literal)]
#![warn(clippy::map_err_ignore)]
#![warn(clippy::mem_forget)]
#![warn(clippy::multiple_inherent_impl)]
#![warn(clippy::pattern_type_mismatch)]
#![warn(clippy::rc_buffer)]
#![warn(clippy::rc_mutex)]
#![warn(clippy::rest_pat_in_fully_bound_structs)]
#![warn(clippy::same_name_method)]
#![warn(clippy::self_named_module_files)]
#![warn(clippy::str_to_string)]
#![warn(clippy::string_to_string)]
#![warn(clippy::undocumented_unsafe_blocks)]
#![warn(clippy::unnecessary_self_imports)]
#![warn(clippy::unneeded_field_pattern)]
#![warn(clippy::use_debug)]
#![warn(clippy::verbose_file_reads)]
// For binary-only crates
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

pub mod parser {
    pub mod frame;
    pub mod grain;
    pub mod obu;
    pub mod sequence;
    pub mod tile_group;
    pub mod util;
}
pub mod reader;

use std::{env, path::PathBuf};

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use parser::{frame::FrameHeader, grain::FilmGrainHeader, sequence::SequenceHeader};

use crate::{
    parser::obu::{parse_obu, Obu},
    reader::BitstreamReader,
};

pub fn main() -> Result<()> {
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "error,grav1synth=info");
    }
    pretty_env_logger::init();

    let args = Args::parse();

    match args.command {
        Commands::Inspect { input, output } => {
            let mut parser = BitstreamReader::open(&input)?;
            let mut size = 0usize;
            let mut seen_frame_header = false;
            let mut sequence_header = None;
            let mut previous_frame_header = None;
            let mut grain_headers = Vec::new();
            while let Some(packet) = parser.read_packet() {
                get_grain_headers(
                    packet.data().unwrap(),
                    &mut size,
                    &mut seen_frame_header,
                    &mut sequence_header,
                    &mut previous_frame_header,
                    &mut grain_headers,
                )?;
            }

            if !grain_headers
                .iter()
                .any(|h| matches!(h, &FilmGrainHeader::UpdateGrain(_)))
            {
                eprintln!("No film grain headers found--this video does not use grain synthesis");
                return Ok(());
            }

            dbg!(&grain_headers);

            todo!("Aggregate the grain info and convert them to table format")
        }
        Commands::Apply {
            input,
            output,
            grain,
        } => todo!(),
        Commands::Remove { input, output } => todo!(),
    }

    Ok(())
}

fn get_grain_headers<'a, 'b>(
    mut input: &'a [u8],
    size: &'b mut usize,
    seen_frame_header: &'b mut bool,
    sequence_header: &'b mut Option<SequenceHeader>,
    previous_frame_header: &'b mut Option<FrameHeader>,
    grain_headers: &'b mut Vec<FilmGrainHeader>,
) -> Result<()> {
    loop {
        let (inner_input, obu) = parse_obu(
            input,
            size,
            seen_frame_header,
            sequence_header.as_ref(),
            previous_frame_header.as_ref(),
        )
        .map_err(|e| anyhow!("{}", e.to_string()))?;
        input = inner_input;
        match obu {
            Some(Obu::SequenceHeader(obu)) => {
                *sequence_header = Some(obu);
            }
            Some(Obu::FrameHeader(obu)) => {
                grain_headers.push(obu.film_grain_params.clone());
                *previous_frame_header = Some(obu);
            }
            None => (),
        };
        if input.is_empty() {
            break;
        }
    }

    Ok(())
}

#[derive(Parser, Debug)]
pub struct Args {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Outputs a film grain table corresponding to a given AV1 video,
    /// or reports if there is no film grain information.
    Inspect {
        /// The AV1 file to inspect.
        #[clap(value_parser)]
        input: PathBuf,
        /// The path to the output film grain table.
        #[clap(long, short, value_parser)]
        output: PathBuf,
    },
    /// Applies film grain from a table file to a given AV1 video,
    /// and outputs it at a given `output` path.
    Apply {
        /// The AV1 file to apply grain to.
        #[clap(value_parser)]
        input: PathBuf,
        /// The path to write the grain-synthed AV1 file to.
        #[clap(long, short, value_parser)]
        output: PathBuf,
        /// The path to the input film grain table.
        #[clap(long, short, value_parser)]
        grain: PathBuf,
    },
    /// Removes all film grain from a given AV1 video,
    /// and outputs it at a given `output` path.
    Remove {
        /// The AV1 file to remove grain from.
        #[clap(value_parser)]
        input: PathBuf,
        /// The path to write the non-grain-synthed AV1 file to.
        #[clap(long, short, value_parser)]
        output: PathBuf,
    },
}
