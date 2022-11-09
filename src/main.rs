#![deny(clippy::all)]
#![warn(clippy::nursery)]
#![warn(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::default_trait_access)]
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
#![warn(clippy::mod_module_files)]
#![warn(clippy::multiple_inherent_impl)]
#![warn(clippy::pattern_type_mismatch)]
#![warn(clippy::rc_buffer)]
#![warn(clippy::rc_mutex)]
#![warn(clippy::rest_pat_in_fully_bound_structs)]
#![warn(clippy::same_name_method)]
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

pub mod parser;
pub mod reader;

use std::{
    env,
    fs::{read_to_string, File},
    io::{BufWriter, Write},
    path::PathBuf,
};

use anyhow::{bail, Result};
#[cfg(feature = "unstable")]
use av1_grain::estimate_plane_noise;
use av1_grain::{generate_photon_noise_params, parse_grain_table, DiffGenerator, TransferFunction};
use clap::{Parser, Subcommand};
use dialoguer::Confirm;
use ffmpeg::{format, sys::AVColorTransferCharacteristic, Rational};
use log::{debug, error, info, warn};
use parser::grain::{FilmGrainHeader, FilmGrainParams};

use crate::{parser::BitstreamParser, reader::BitstreamReader};

#[allow(clippy::too_many_lines)]
#[allow(clippy::cognitive_complexity)]
pub fn main() -> Result<()> {
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "error,grav1synth=info");
    }
    pretty_env_logger::init();

    let args = Args::parse();

    match args.command {
        Commands::Inspect {
            input,
            output,
            overwrite,
        } => {
            if input == output {
                error!(
                    "Input and output paths are the same. This is probably a typo, because this \
                     would overwrite your input. Exiting."
                );
                return Ok(());
            }

            if output.exists()
                && !overwrite
                && !Confirm::new()
                    .with_prompt(format!(
                        "File {} exists. Overwrite?",
                        output.to_string_lossy()
                    ))
                    .interact()?
            {
                warn!("Not overwriting existing file. Exiting.");
                return Ok(());
            }

            let reader = BitstreamReader::open(&input)?;
            let frame_rate = reader.get_video_details().frame_rate;
            let mut parser: BitstreamParser<false> = BitstreamParser::new(reader);
            let grain_headers = parser.get_grain_headers()?;

            if !grain_headers
                .iter()
                .any(|h| matches!(h, &FilmGrainHeader::UpdateGrain(_)))
            {
                info!("No film grain headers found--this video does not use grain synthesis");
                return Ok(());
            }

            // As you can expect, this may lead to odd behaviors with VFR.
            // VFR is cursed.
            let grain_tables = aggregate_grain_headers(grain_headers, frame_rate);

            let mut output_file = BufWriter::new(File::create(&output)?);
            writeln!(&mut output_file, "filmgrn1")?;
            for segment in grain_tables {
                write_film_grain_segment(&segment, &mut output_file)?;
            }
            output_file.flush()?;

            info!("Done, wrote grain table to {}", output.to_string_lossy());
        }
        Commands::Apply {
            input,
            output,
            overwrite,
            grain,
        } => {
            if input == output {
                error!(
                    "Input and output paths are the same. This is probably a typo, because this \
                     would overwrite your input. Exiting."
                );
                return Ok(());
            }

            if output.exists()
                && !overwrite
                && !Confirm::new()
                    .with_prompt(format!(
                        "File {} exists. Overwrite?",
                        output.to_string_lossy()
                    ))
                    .interact()?
            {
                warn!("Not overwriting existing file. Exiting.");
                return Ok(());
            }

            let reader = BitstreamReader::open(&input)?;
            let writer = format::output(&output)?;
            let grain_data = read_to_string(&grain)?;
            let new_headers = parse_grain_table(&grain_data)?;
            let mut parser: BitstreamParser<true> = BitstreamParser::with_writer(
                reader,
                writer,
                Some(
                    new_headers
                        .into_iter()
                        .map(|h| h.into())
                        .collect::<Vec<_>>(),
                ),
            );

            parser.modify_grain_headers()?;

            info!("Done, wrote output file to {}", output.to_string_lossy());
        }
        Commands::Generate {
            input,
            output,
            overwrite,
            iso,
            chroma,
        } => {
            if input == output {
                error!(
                    "Input and output paths are the same. This is probably a typo, because this \
                     would overwrite your input. Exiting."
                );
                return Ok(());
            }

            if output.exists()
                && !overwrite
                && !Confirm::new()
                    .with_prompt(format!(
                        "File {} exists. Overwrite?",
                        output.to_string_lossy()
                    ))
                    .interact()?
            {
                warn!("Not overwriting existing file. Exiting.");
                return Ok(());
            }

            let reader = BitstreamReader::open(&input)?;
            let writer = format::output(&output)?;
            let video_stream = reader.get_video_stream().unwrap();
            // SAFETY: We immediately dereference the pointer to get the contained struct,
            // so there's no possibility of use-after-free later.
            let video_params = unsafe { *video_stream.parameters().as_ptr() };
            let width = video_params.width as u32;
            let height = video_params.height as u32;
            let trc = video_params.color_trc;
            let grain_data = generate_photon_noise_params(
                0,
                u64::MAX,
                av1_grain::NoiseGenArgs {
                    iso_setting: u32::from(iso),
                    width,
                    height,
                    transfer_function: if trc == AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084
                    {
                        TransferFunction::SMPTE2084
                    } else {
                        TransferFunction::BT1886
                    },
                    chroma_grain: chroma,
                    random_seed: None,
                },
            );
            let mut parser: BitstreamParser<true> =
                BitstreamParser::with_writer(reader, writer, Some(vec![grain_data.into()]));

            parser.modify_grain_headers()?;

            info!("Done, wrote output file to {}", output.to_string_lossy());
        }
        Commands::Remove {
            input,
            output,
            overwrite,
        } => {
            if input == output {
                error!(
                    "Input and output paths are the same. This is probably a typo, because this \
                     would overwrite your input. Exiting."
                );
                return Ok(());
            }

            if output.exists()
                && !overwrite
                && !Confirm::new()
                    .with_prompt(format!(
                        "File {} exists. Overwrite?",
                        output.to_string_lossy()
                    ))
                    .interact()?
            {
                warn!("Not overwriting existing file. Exiting.");
                return Ok(());
            }

            let reader = BitstreamReader::open(&input)?;
            let writer = format::output(&output)?;
            let mut parser: BitstreamParser<true> =
                BitstreamParser::with_writer(reader, writer, None);

            parser.modify_grain_headers()?;

            info!("Done, wrote output file to {}", output.to_string_lossy());
        }
        Commands::Diff {
            source,
            denoised,
            output,
            overwrite,
        } => {
            if source == output || denoised == output {
                error!(
                    "Input and output paths are the same. This is probably a typo, because this \
                     would overwrite your input. Exiting."
                );
                return Ok(());
            }

            if source == denoised {
                error!(
                    "Source and denoised paths are the same. This is probably a typo, because \
                     this would always compute an empty diff. Exiting."
                );
                return Ok(());
            }

            if output.exists()
                && !overwrite
                && !Confirm::new()
                    .with_prompt(format!(
                        "File {} exists. Overwrite?",
                        output.to_string_lossy()
                    ))
                    .interact()?
            {
                warn!("Not overwriting existing file. Exiting.");
                return Ok(());
            }

            let mut source_reader = BitstreamReader::open(&source)?;
            let mut denoised_reader = BitstreamReader::open(&denoised)?;
            let frame_rate = source_reader.get_video_details().frame_rate;
            let source_bd = source_reader.get_video_details().bit_depth;
            let denoised_bd = denoised_reader.get_video_details().bit_depth;
            let mut differ = DiffGenerator::new(
                num_rational::Rational64::new(
                    i64::from(frame_rate.numerator()),
                    i64::from(frame_rate.denominator()),
                ),
                source_bd,
                denoised_bd,
            );
            let mut frames = 0usize;

            loop {
                debug!("Diffing next frame");
                match (source_bd, denoised_bd) {
                    (8, 8) => match (
                        source_reader.get_frame::<u8>()?,
                        denoised_reader.get_frame::<u8>()?,
                    ) {
                        (Some(source_frame), Some(denoised_frame)) => {
                            differ.diff_frame(&source_frame, &denoised_frame)?;
                        }
                        (None, None) => {
                            break;
                        }
                        _ => {
                            warn!(
                                "Videos did not have equal frame counts. Resulting grain table \
                                 may not be as expected."
                            );
                            break;
                        }
                    },
                    (8, 9..=16) => match (
                        source_reader.get_frame::<u8>()?,
                        denoised_reader.get_frame::<u16>()?,
                    ) {
                        (Some(source_frame), Some(denoised_frame)) => {
                            differ.diff_frame(&source_frame, &denoised_frame)?;
                        }
                        (None, None) => {
                            break;
                        }
                        _ => {
                            warn!(
                                "Videos did not have equal frame counts. Resulting grain table \
                                 may not be as expected."
                            );
                            break;
                        }
                    },
                    (9..=16, 8) => match (
                        source_reader.get_frame::<u16>()?,
                        denoised_reader.get_frame::<u8>()?,
                    ) {
                        (Some(source_frame), Some(denoised_frame)) => {
                            differ.diff_frame(&source_frame, &denoised_frame)?;
                        }
                        (None, None) => {
                            break;
                        }
                        _ => {
                            warn!(
                                "Videos did not have equal frame counts. Resulting grain table \
                                 may not be as expected."
                            );
                            break;
                        }
                    },
                    (9..=16, 9..=16) => match (
                        source_reader.get_frame::<u16>()?,
                        denoised_reader.get_frame::<u16>()?,
                    ) {
                        (Some(source_frame), Some(denoised_frame)) => {
                            differ.diff_frame(&source_frame, &denoised_frame)?;
                        }
                        (None, None) => {
                            break;
                        }
                        _ => {
                            warn!(
                                "Videos did not have equal frame counts. Resulting grain table \
                                 may not be as expected."
                            );
                            break;
                        }
                    },
                    _ => {
                        bail!("Bit depths not between 8-16 are not currently supported");
                    }
                }
                frames += 1;
            }

            let grain_tables = differ.finish();
            let mut output_file = BufWriter::new(File::create(&output)?);
            writeln!(&mut output_file, "filmgrn1")?;
            for segment in grain_tables {
                write_film_grain_segment(&segment.into(), &mut output_file)?;
            }
            output_file.flush()?;
            info!("Computed diff for {} frames", frames);
            info!("Done, wrote output file to {}", output.to_string_lossy());
        }
        #[cfg(feature = "unstable")]
        Commands::Estimate {
            source,
            output,
            overwrite,
            chroma,
        } => {
            if source == output {
                error!(
                    "Input and output paths are the same. This is probably a typo, because this \
                     would overwrite your input. Exiting."
                );
                return Ok(());
            }

            if output.exists()
                && !overwrite
                && !Confirm::new()
                    .with_prompt(format!(
                        "File {} exists. Overwrite?",
                        output.to_string_lossy()
                    ))
                    .interact()?
            {
                warn!("Not overwriting existing file. Exiting.");
                return Ok(());
            }

            let mut reader = BitstreamReader::open(&source)?;
            let bit_depth = reader.get_video_details().bit_depth;
            let mut frame_estimates = Vec::new();

            loop {
                match bit_depth {
                    8 => match reader.get_frame::<u8>()? {
                        Some(frame) => {
                            frame_estimates.push(estimate_plane_noise(&frame.planes[0], bit_depth));
                        }
                        None => {
                            break;
                        }
                    },
                    9..=16 => match reader.get_frame::<u16>()? {
                        Some(frame) => {
                            frame_estimates.push(estimate_plane_noise(&frame.planes[0], bit_depth));
                        }
                        None => {
                            break;
                        }
                    },
                    _ => {
                        bail!("Bit depths not between 8-16 are not currently supported");
                    }
                }
            }

            let video_stream = reader.get_video_stream().unwrap();
            // SAFETY: We immediately dereference the pointer to get the contained struct,
            // so there's no possibility of use-after-free later.
            let video_params = unsafe { *video_stream.parameters().as_ptr() };
            let frame_rate = reader.get_video_details().frame_rate;
            let trc = video_params.color_trc;

            let mut output_file = BufWriter::new(File::create(&output)?);
            writeln!(&mut output_file, "filmgrn1")?;
            for estimate in &frame_estimates {
                writeln!(&mut output_file, "{:.3}", estimate.unwrap_or(-1f64))?;
            }
            // for segment in build_segments_from_estimate(&frame_estimates, video_params,
            // frame_rate, chroma) {
            //     write_film_grain_segment(&segment.into(), &mut output_file)?;
            // }
            output_file.flush()?;
            info!("Done, wrote output file to {}", output.to_string_lossy());
        }
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
pub struct GrainTableSegment {
    pub start_time: u64,
    pub end_time: u64,
    pub grain_params: FilmGrainParams,
}

impl From<av1_grain::GrainTableSegment> for GrainTableSegment {
    fn from(data: av1_grain::GrainTableSegment) -> Self {
        GrainTableSegment {
            start_time: data.start_time,
            end_time: data.end_time,
            grain_params: data.into(),
        }
    }
}

// I don't know why this is the base unit for a timestamp but it is. 1/10000000
// of a second.
const TIMESTAMP_BASE_UNIT: u64 = 10_000_000;

fn aggregate_grain_headers(
    grain_headers: &[FilmGrainHeader],
    frame_rate: Rational,
) -> Vec<GrainTableSegment> {
    let time_per_packet: f64 = frame_rate.invert().into();
    let mut cur_packet_start: u64 = 0;
    let mut cur_packet_end_f: f64 = time_per_packet;
    let mut cur_packet_end: u64 = cur_packet_end_f.ceil() as u64 * TIMESTAMP_BASE_UNIT;

    grain_headers.iter().fold(Vec::new(), |mut acc, elem| {
        let prev_packet_has_grain = acc.last().map_or(false, |last: &GrainTableSegment| {
            last.end_time == cur_packet_start
        });
        if prev_packet_has_grain {
            match *elem {
                FilmGrainHeader::Disable => {
                    // Do nothing. This will disable film grain for this
                    // and future frames.
                }
                FilmGrainHeader::CopyRefFrame => {
                    // Increment the end time of the current table segment.
                    let cur_segment = acc.last_mut().expect("prev_packet_has_grain is true");
                    cur_segment.end_time = cur_packet_end;
                }
                FilmGrainHeader::UpdateGrain(ref grain_params) => {
                    let cur_segment = acc.last_mut().expect("prev_packet_has_grain is true");
                    if grain_params == &cur_segment.grain_params {
                        // Increment the end time of the current table segment.
                        cur_segment.end_time = cur_packet_end;
                    } else {
                        // The grain params changed, so we have to make a new segment.
                        acc.push(GrainTableSegment {
                            start_time: cur_packet_start,
                            end_time: cur_packet_end,
                            grain_params: grain_params.clone(),
                        });
                    }
                }
            };
        } else if let &FilmGrainHeader::UpdateGrain(ref grain_params) = elem {
            acc.push(GrainTableSegment {
                start_time: cur_packet_start,
                end_time: cur_packet_end,
                grain_params: grain_params.clone(),
            });
        }

        cur_packet_start = cur_packet_end;
        cur_packet_end_f += time_per_packet;
        cur_packet_end = cur_packet_end_f.ceil() as u64 * TIMESTAMP_BASE_UNIT;
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
        /// Overwrite the output file without prompting.
        #[clap(long, short = 'y')]
        overwrite: bool,
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
        /// Overwrite the output file without prompting.
        #[clap(long, short = 'y')]
        overwrite: bool,
        /// The path to the input film grain table.
        #[clap(long, short, value_parser)]
        grain: PathBuf,
    },
    /// Generates photon-noise-based film grain based on a given ISO value,
    /// adds it to a given AV1 video, and outputs it at a given `output` path.
    Generate {
        /// The AV1 file to apply grain to.
        #[clap(value_parser)]
        input: PathBuf,
        /// The path to write the grain-synthed AV1 file to.
        #[clap(long, short, value_parser)]
        output: PathBuf,
        /// Overwrite the output file without prompting.
        #[clap(long, short = 'y')]
        overwrite: bool,
        /// ISO strength (100-6400) for the generated grain.
        #[clap(long, value_parser = clap::value_parser!(u16).range(1..=6400))]
        iso: u16,
        /// Whether to apply grain to the chroma planes as well.
        #[clap(long)]
        chroma: bool,
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
        /// Overwrite the output file without prompting.
        #[clap(long, short = 'y')]
        overwrite: bool,
    },
    /// Compares a source video and a denoised video and generates a film grain
    /// table based on the difference between them. This will provide the most
    /// accurate estimation of source film grain.
    Diff {
        /// The untouched source file to inspect.
        #[clap(value_parser)]
        source: PathBuf,
        /// The denoised file to inspect.
        #[clap(value_parser)]
        denoised: PathBuf,
        /// The path to the output film grain table.
        #[clap(long, short, value_parser)]
        output: PathBuf,
        /// Overwrite the output file without prompting.
        #[clap(long, short = 'y')]
        overwrite: bool,
    },
    /// Analyzes a source video and estimates the amount of noise in the source,
    /// then generates an appropriate film grain table. This is less accurate
    /// than the diff method, but is significantly faster.
    #[cfg(feature = "unstable")]
    Estimate {
        /// The source file to inspect.
        #[clap(value_parser)]
        source: PathBuf,
        /// The path to the output film grain table.
        #[clap(long, short, value_parser)]
        output: PathBuf,
        /// Overwrite the output file without prompting.
        #[clap(long, short = 'y')]
        overwrite: bool,
        /// Whether to apply grain to the chroma planes as well.
        #[clap(long)]
        chroma: bool,
    },
}
