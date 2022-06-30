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
    pub mod util;
}

use std::{env, path::PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::parser::grain::{get_grain_header, FilmGrainParser};

pub fn main() -> Result<()> {
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "error,grav1synth=info");
    }
    pretty_env_logger::init();

    let args = Args::parse();

    match args.command {
        Commands::Inspect { input, output } => {
            assert!(
                input.extension().unwrap().to_ascii_lowercase() == "ivf",
                "Currently, only .ivf input is supported"
            );
            assert!(
                output.extension().unwrap().to_ascii_lowercase() == "ivf",
                "Currently, only .ivf output is supported"
            );

            let mut parser = FilmGrainParser::open(&input)?;
            let video_headers = parser.get_headers();
            let mut grain_headers = Vec::new();
            while let Some(packet) = parser.read_packet()? {
                grain_headers.push(get_grain_header(&packet)?);
            }

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
