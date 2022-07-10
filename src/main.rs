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

use std::{
    env,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
};

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use dialoguer::Confirm;
use ffmpeg::Rational;
use parser::{
    frame::{FrameHeader, RefType, NUM_REF_FRAMES, REFS_PER_FRAME},
    grain::{FilmGrainHeader, FilmGrainParams},
    sequence::SequenceHeader,
};

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
            if output.exists()
                && !Confirm::new()
                    .with_prompt(format!(
                        "File {} exists. Overwrite?",
                        output.to_string_lossy()
                    ))
                    .interact()?
            {
                eprintln!("Not overwriting existing file. Exiting.");
                return Ok(());
            }

            let mut parser = BitstreamReader::open(&input)?;
            let mut size = 0usize;
            let mut seen_frame_header = false;
            let mut sequence_header = None;
            let mut previous_frame_header = None;
            let mut ref_frame_idx = [0usize; REFS_PER_FRAME];
            let mut ref_order_hint = [0u64; NUM_REF_FRAMES];
            let mut big_ref_order_hint = [0u64; NUM_REF_FRAMES];
            let mut big_ref_valid = [false; NUM_REF_FRAMES];
            let mut big_order_hints = [0u64; RefType::Last as usize + REFS_PER_FRAME];
            let mut grain_headers = Vec::new();
            while let Some(packet) = parser.read_packet() {
                if packet.data().is_none() {
                    break;
                }
                get_grain_headers(
                    packet.data().unwrap(),
                    &mut size,
                    &mut seen_frame_header,
                    &mut sequence_header,
                    &mut previous_frame_header,
                    &mut grain_headers,
                    &mut ref_frame_idx,
                    &mut ref_order_hint,
                    &mut big_ref_order_hint,
                    &mut big_ref_valid,
                    &mut big_order_hints,
                )?;
            }

            if !grain_headers
                .iter()
                .any(|h| matches!(h, &FilmGrainHeader::UpdateGrain(_)))
            {
                eprintln!("No film grain headers found--this video does not use grain synthesis");
                return Ok(());
            }

            // As you can expect, this may lead to odd behaviors with VFR.
            // VFR is cursed.
            let frame_rate = parser.get_video_stream()?.avg_frame_rate();
            let grain_tables = aggregate_grain_headers(grain_headers, frame_rate);

            let mut output_file = BufWriter::new(File::create(&output)?);
            writeln!(&mut output_file, "filmgrn1")?;
            for segment in grain_tables {
                write_film_grain_segment(&segment, &mut output_file)?;
            }
            output_file.flush()?;

            eprintln!("Done, wrote grain table to {}", output.to_string_lossy());
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

fn write_film_grain_segment(
    segment: &GrainTableSegment,
    output: &mut BufWriter<File>,
) -> anyhow::Result<()> {
    let params = &segment.grain_params;

    writeln!(
        output,
        "E {} {} 1 {} 1",
        segment.start_time, segment.end_time, params.grain_seed,
    )?;
    writeln!(
        output,
        "\tp {} {} {} {} {} {} {} {} {} {} {} {}",
        params.ar_coeff_lag,
        params.ar_coeff_shift,
        params.grain_scale_shift,
        params.scaling_shift,
        u8::from(params.chroma_scaling_from_luma),
        u8::from(params.overlap_flag),
        params.cb_mult,
        params.cb_luma_mult,
        params.cb_offset,
        params.cr_mult,
        params.cr_luma_mult,
        params.cr_offset
    )?;

    write!(output, "\tsY {} ", params.scaling_points_y.len())?;
    for point in &params.scaling_points_y {
        write!(output, " {} {}", point[0], point[1])?;
    }
    writeln!(output)?;

    write!(output, "\tsCb {}", params.scaling_points_cb.len())?;
    for point in &params.scaling_points_cb {
        write!(output, " {} {}", point[0], point[1])?;
    }
    writeln!(output)?;

    write!(output, "\tsCr {}", params.scaling_points_cr.len())?;
    for point in &params.scaling_points_cr {
        write!(output, " {} {}", point[0], point[1])?;
    }
    writeln!(output)?;

    write!(output, "\tcY")?;
    for coeff in &params.ar_coeffs_y {
        write!(output, " {}", *coeff)?;
    }
    writeln!(output)?;

    write!(output, "\tcCb")?;
    for coeff in &params.ar_coeffs_cb {
        write!(output, " {}", *coeff)?;
    }
    writeln!(output)?;

    write!(output, "\tcCr")?;
    for coeff in &params.ar_coeffs_cr {
        write!(output, " {}", *coeff)?;
    }
    writeln!(output)?;

    Ok(())
}

#[derive(Debug, Clone)]
struct GrainTableSegment {
    pub start_time: u64,
    pub end_time: u64,
    pub grain_params: FilmGrainParams,
}

#[allow(clippy::too_many_arguments)]
fn get_grain_headers<'a, 'b>(
    mut input: &'a [u8],
    size: &'b mut usize,
    seen_frame_header: &'b mut bool,
    sequence_header: &'b mut Option<SequenceHeader>,
    previous_frame_header: &'b mut Option<FrameHeader>,
    grain_headers: &'b mut Vec<FilmGrainHeader>,
    ref_frame_idx: &mut [usize; REFS_PER_FRAME],
    ref_order_hint: &mut [u64; NUM_REF_FRAMES],
    big_ref_order_hint: &mut [u64; NUM_REF_FRAMES],
    big_ref_valid: &mut [bool; NUM_REF_FRAMES],
    big_order_hints: &mut [u64; RefType::Last as usize + REFS_PER_FRAME],
) -> Result<()> {
    loop {
        let (inner_input, obu) = parse_obu(
            input,
            size,
            seen_frame_header,
            sequence_header.as_ref(),
            previous_frame_header.as_ref(),
            ref_frame_idx,
            ref_order_hint,
            big_ref_order_hint,
            big_ref_valid,
            big_order_hints,
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

fn aggregate_grain_headers(
    grain_headers: Vec<FilmGrainHeader>,
    frame_rate: Rational,
) -> Vec<GrainTableSegment> {
    let time_per_packet: f64 = frame_rate.invert().into();
    let mut cur_packet_start: u64 = 0;
    let mut cur_packet_end_f: f64 = time_per_packet;
    let mut cur_packet_end: u64 = cur_packet_end_f.ceil() as u64;

    grain_headers.into_iter().fold(Vec::new(), |mut acc, elem| {
        let prev_packet_has_grain = acc.last().map_or(false, |last: &GrainTableSegment| {
            last.end_time == cur_packet_start
        });
        if prev_packet_has_grain {
            match elem {
                FilmGrainHeader::Disable => {
                    // Do nothing. This will disable film grain for this
                    // and future frames.
                }
                FilmGrainHeader::CopyRefFrame => {
                    // Increment the end time of the current table segment.
                    let cur_segment = acc.last_mut().expect("prev_packet_has_grain is true");
                    cur_segment.end_time = cur_packet_end;
                }
                FilmGrainHeader::UpdateGrain(grain_params) => {
                    let cur_segment = acc.last_mut().expect("prev_packet_has_grain is true");
                    if grain_params == cur_segment.grain_params {
                        // Increment the end time of the current table segment.
                        cur_segment.end_time = cur_packet_end;
                    } else {
                        // The grain params changed, so we have to make a new segment.
                        acc.push(GrainTableSegment {
                            start_time: cur_packet_start,
                            end_time: cur_packet_end,
                            grain_params,
                        });
                    }
                }
            };
        } else if let FilmGrainHeader::UpdateGrain(grain_params) = elem {
            acc.push(GrainTableSegment {
                start_time: cur_packet_start,
                end_time: cur_packet_end,
                grain_params,
            });
        }

        cur_packet_start = cur_packet_end;
        cur_packet_end_f += time_per_packet;
        cur_packet_end = cur_packet_end_f.ceil() as u64;
        acc
    })
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
