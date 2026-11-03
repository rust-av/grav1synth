use arrayvec::ArrayVec;
use bit::BitIndex;
use log::{debug, trace};
use nom::{IResult, bits::bits, error::Error};
use num_enum::TryFromPrimitive;

use crate::misc::to_binary_string;

use super::{
    BitstreamParser,
    trace::{
        TraceCtx, trace_bool, trace_field, trace_take_u8, trace_take_u16, trace_take_u32,
        trace_take_u64, trace_take_usize,
    },
    util::{BitInput, uvlc},
};

pub const SELECT_SCREEN_CONTENT_TOOLS: u8 = 2;
pub const SELECT_INTEGER_MV: u8 = 2;

#[derive(Debug, Clone)]
pub struct SequenceHeader {
    pub reduced_still_picture_header: bool,
    pub frame_id_numbers_present: bool,
    pub additional_frame_id_len_minus_1: usize,
    pub delta_frame_id_len_minus_2: usize,
    pub film_grain_params_present: bool,
    pub new_film_grain_state: bool,
    pub force_screen_content_tools: u8,
    pub force_integer_mv: u8,
    pub order_hint_bits: usize,
    pub frame_width_bits_minus_1: usize,
    pub frame_height_bits_minus_1: usize,
    pub max_frame_width_minus_1: u32,
    pub max_frame_height_minus_1: u32,
    pub decoder_model_info: Option<DecoderModelInfo>,
    pub decoder_model_present_for_op: ArrayVec<bool, { 1 << 5u8 }>,
    pub operating_points_cnt_minus_1: usize,
    pub operating_point_idc: ArrayVec<u16, { 1 << 5u8 }>,
    pub cur_operating_point_idc: u16,
    pub timing_info: Option<TimingInfo>,
    pub enable_ref_frame_mvs: bool,
    pub enable_warped_motion: bool,
    pub enable_superres: bool,
    pub enable_cdef: bool,
    pub enable_restoration: bool,
    pub use_128x128_superblock: bool,
    pub color_config: ColorConfig,
}

impl SequenceHeader {
    /// Returns whether sequence-level order hints are enabled.
    ///
    /// AV1 signals this as a bit width (`order_hint_bits`). A width of `0`
    /// means order hints are globally disabled for the stream.
    #[must_use]
    pub const fn enable_order_hint(&self) -> bool {
        self.order_hint_bits > 0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TimingInfo {
    pub equal_picture_interval: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct DecoderModelInfo {
    pub buffer_delay_length_minus_1: u8,
    pub buffer_removal_time_length_minus_1: u8,
    pub frame_presentation_time_length_minus_1: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct ColorConfig {
    pub color_primaries: ColorPrimaries,
    pub transfer_characteristics: TransferCharacteristics,
    pub matrix_coefficients: MatrixCoefficients,
    pub color_range: ColorRange,
    pub num_planes: u8,
    pub separate_uv_delta_q: bool,
    pub subsampling: (u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum ColorRange {
    Limited = 0,
    Full = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum ColorPrimaries {
    Bt709 = 1,
    Unspecified = 2,
    Bt470m = 4,
    Bt470bg = 5,
    Bt601 = 6,
    Smpte240 = 7,
    Film = 8,
    Bt2020 = 9,
    Xyz = 10,
    Smpte431 = 11,
    Smpte432 = 12,
    Ebu3213 = 22,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum TransferCharacteristics {
    Reserved0 = 0,
    Bt709 = 1,
    Unspecified = 2,
    Reserved3 = 3,
    Bt470m = 4,
    Bt470bg = 5,
    Bt601 = 6,
    Smpte240 = 7,
    Linear = 8,
    Log100 = 9,
    Log100Sqrt10 = 10,
    Iec61966 = 11,
    Bt1361 = 12,
    Srgb = 13,
    Bt2020_10Bit = 14,
    Bt2020_12Bit = 15,
    Smpte2084 = 16,
    Smpte428 = 17,
    Hlg = 18,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum MatrixCoefficients {
    Identity = 0,
    Bt709 = 1,
    Unspecified = 2,
    Reserved3 = 3,
    Fcc = 4,
    Bt470bg = 5,
    Bt601 = 6,
    Smpte240 = 7,
    SmpteYCgCo = 8,
    Bt2020Ncl = 9,
    Bt2020Cl = 10,
    Smpte2085 = 11,
    ChromaticityNcl = 12,
    ChromaticityCl = 13,
    ICtCp = 14,
}

impl<const WRITE: bool> BitstreamParser<WRITE> {
    #[allow(clippy::too_many_lines)]
    /// Parses an AV1 sequence header OBU payload into [`SequenceHeader`].
    ///
    /// CONTRACT: `input` must begin at the first bit of the sequence header
    /// payload (after OBU framing has been handled by the caller).
    ///
    /// In write mode (`WRITE = true`), this parser mirrors the consumed OBU
    /// bytes and rewrites only the `film_grain_params_present` bit so it
    /// matches whether incoming grain data is being applied.
    pub fn parse_sequence_header<'a>(
        &mut self,
        input: &'a [u8],
        obu_bit_offset: usize,
    ) -> IResult<&'a [u8], SequenceHeader, Error<&'a [u8]>> {
        let mut obu_out = if WRITE {
            input[..self.size].to_owned()
        } else {
            Vec::new()
        };
        bits(move |input| {
            let ctx = TraceCtx::new(input, obu_bit_offset);
            let (input, seq_profile) = trace_take_u8(input, ctx, 3, "seq_profile")?;
            let (input, _still_picture) = trace_bool(input, ctx, "still_picture")?;
            let (input, reduced_still_picture_header) =
                trace_bool(input, ctx, "reduced_still_picture_header")?;
            let (
                input,
                decoder_model_info,
                operating_points_cnt_minus_1,
                decoder_model_present_for_op,
                operating_point_idc,
                timing_info,
            ) = if reduced_still_picture_header {
                let (input, _seq_level_idx) = trace_take_u8(input, ctx, 5, "seq_level_idx[0]")?;
                // AV1 spec: reduced_still_picture_header implies a single
                // operating point with idc=0 and no decoder model.
                let mut op_idc = ArrayVec::new();
                op_idc.push(0);
                let mut dm_present = ArrayVec::new();
                dm_present.push(false);
                (input, None, 0, dm_present, op_idc, None)
            } else {
                let (input, timing_info_present_flag) =
                    trace_bool(input, ctx, "timing_info_present_flag")?;
                let (input, decoder_model_info, timing_info) = if timing_info_present_flag {
                    let (input, timing_info) = timing_info(input, ctx)?;
                    let (input, flag) = trace_bool(input, ctx, "decoder_model_info_present_flag")?;
                    let (input, decoder_model, timing_info) = if flag {
                        let (input, decoder_model) = decoder_model_info(input, ctx)?;
                        (input, Some(decoder_model), timing_info)
                    } else {
                        (input, None, timing_info)
                    };
                    (input, decoder_model, Some(timing_info))
                } else {
                    (input, None, None)
                };
                let (input, initial_display_delay_present_flag) =
                    trace_bool(input, ctx, "initial_display_delay_present_flag")?;

                let mut decoder_model_present_for_op = ArrayVec::new();
                let mut operating_point_idc = ArrayVec::new();
                let (mut input, operating_points_cnt_minus_1) =
                    trace_take_usize(input, ctx, 5, "operating_points_cnt_minus_1")?;
                for i in 0..=operating_points_cnt_minus_1 {
                    let inner_input = input;
                    let (inner_input, cur_operating_point_idc) =
                        trace_take_u16(inner_input, ctx, 12, &format!("operating_point_idc[{i}]"))?;
                    operating_point_idc.push(cur_operating_point_idc);
                    let (inner_input, seq_level_idx) =
                        trace_take_u8(inner_input, ctx, 5, &format!("seq_level_idx[{i}]"))?;
                    let (inner_input, _seq_tier) = if seq_level_idx > 7 {
                        trace_bool(inner_input, ctx, &format!("seq_tier[{i}]"))?
                    } else {
                        (inner_input, false)
                    };
                    let (inner_input, cur_decoder_model_present_for_op) =
                        if let Some(decoder_model_info) = decoder_model_info {
                            let (inner_input, flag) = trace_bool(
                                inner_input,
                                ctx,
                                &format!("decoder_model_present_for_this_op[{i}]"),
                            )?;
                            if flag {
                                (
                                    operating_parameters_info(
                                        inner_input,
                                        ctx,
                                        decoder_model_info.buffer_delay_length_minus_1 as usize + 1,
                                    )?
                                    .0,
                                    flag,
                                )
                            } else {
                                (inner_input, flag)
                            }
                        } else {
                            (inner_input, false)
                        };
                    decoder_model_present_for_op.push(cur_decoder_model_present_for_op);
                    let (inner_input, _initial_display_delay_present_for_op) =
                        if initial_display_delay_present_flag {
                            let (inner_input, flag) = trace_bool(
                                inner_input,
                                ctx,
                                &format!("initial_display_delay_present_for_this_op[{i}]"),
                            )?;
                            if flag {
                                let (inner_input, _initial_display_delay_minus_1) = trace_take_u8(
                                    inner_input,
                                    ctx,
                                    4,
                                    &format!("initial_display_delay_minus_1[{i}]"),
                                )?;
                                (inner_input, flag)
                            } else {
                                (inner_input, flag)
                            }
                        } else {
                            (inner_input, false)
                        };
                    input = inner_input;
                }
                (
                    input,
                    decoder_model_info,
                    operating_points_cnt_minus_1,
                    decoder_model_present_for_op,
                    operating_point_idc,
                    timing_info,
                )
            };

            let operating_point = choose_operating_point();
            let cur_operating_point_idc = operating_point_idc[operating_point];
            let (input, frame_width_bits_minus_1) =
                trace_take_usize(input, ctx, 4, "frame_width_bits_minus_1")?;
            let (input, frame_height_bits_minus_1) =
                trace_take_usize(input, ctx, 4, "frame_height_bits_minus_1")?;
            let (input, max_frame_width_minus_1) = trace_take_u32(
                input,
                ctx,
                frame_width_bits_minus_1 + 1,
                "max_frame_width_minus_1",
            )?;
            let (input, max_frame_height_minus_1) = trace_take_u32(
                input,
                ctx,
                frame_height_bits_minus_1 + 1,
                "max_frame_height_minus_1",
            )?;
            let (input, frame_id_numbers_present) = if reduced_still_picture_header {
                (input, false)
            } else {
                trace_bool(input, ctx, "frame_id_numbers_present_flag")?
            };
            let (input, delta_frame_id_len_minus_2, additional_frame_id_len_minus_1) =
                if frame_id_numbers_present {
                    let (input, delta_frame_id_len_minus_2) =
                        trace_take_usize(input, ctx, 4, "delta_frame_id_length_minus_2")?;
                    let (input, additional_frame_id_len_minus_1) =
                        trace_take_usize(input, ctx, 3, "additional_frame_id_length_minus_1")?;
                    (
                        input,
                        delta_frame_id_len_minus_2,
                        additional_frame_id_len_minus_1,
                    )
                } else {
                    (input, 0, 0)
                };
            let (input, use_128x128_superblock) = trace_bool(input, ctx, "use_128x128_superblock")?;
            let (input, _enable_filter_intra) = trace_bool(input, ctx, "enable_filter_intra")?;
            let (input, _enable_intra_edge_filter) =
                trace_bool(input, ctx, "enable_intra_edge_filter")?;
            let (
                input,
                force_screen_content_tools,
                force_integer_mv,
                order_hint_bits,
                enable_ref_frame_mvs,
                enable_warped_motion,
            ) = if reduced_still_picture_header {
                (
                    input,
                    SELECT_SCREEN_CONTENT_TOOLS,
                    SELECT_INTEGER_MV,
                    0,
                    false,
                    false,
                )
            } else {
                let (input, _enable_interintra_compound) =
                    trace_bool(input, ctx, "enable_interintra_compound")?;
                let (input, _enable_masked_compound) =
                    trace_bool(input, ctx, "enable_masked_compound")?;
                let (input, enable_warped_motion) = trace_bool(input, ctx, "enable_warped_motion")?;
                let (input, _enable_dual_filter) = trace_bool(input, ctx, "enable_dual_filter")?;
                let (input, enable_order_hint) = trace_bool(input, ctx, "enable_order_hint")?;
                let (input, enable_ref_frame_mvs) = if enable_order_hint {
                    let (input, _enable_jnt_comp) = trace_bool(input, ctx, "enable_jnt_comp")?;
                    let (input, enable_ref_frame_mvs) =
                        trace_bool(input, ctx, "enable_ref_frame_mvs")?;
                    (input, enable_ref_frame_mvs)
                } else {
                    (input, false)
                };
                let (input, seq_choose_screen_content_tools) =
                    trace_bool(input, ctx, "seq_choose_screen_content_tools")?;
                let (input, seq_force_screen_content_tools): (_, u8) =
                    if seq_choose_screen_content_tools {
                        (input, SELECT_SCREEN_CONTENT_TOOLS)
                    } else {
                        trace_take_u8(input, ctx, 1, "seq_force_screen_content_tools")?
                    };

                let (input, seq_force_integer_mv) = if seq_force_screen_content_tools > 0 {
                    let (input, seq_choose_integer_mv) =
                        trace_bool(input, ctx, "seq_choose_integer_mv")?;
                    if seq_choose_integer_mv {
                        (input, SELECT_INTEGER_MV)
                    } else {
                        trace_take_u8(input, ctx, 1, "seq_force_integer_mv")?
                    }
                } else {
                    (input, SELECT_INTEGER_MV)
                };
                let (input, order_hint_bits) = if enable_order_hint {
                    let (input, order_hint_bits_minus_1) =
                        trace_take_usize(input, ctx, 3, "order_hint_bits_minus_1")?;
                    (input, order_hint_bits_minus_1 + 1)
                } else {
                    (input, 0)
                };

                (
                    input,
                    seq_force_screen_content_tools,
                    seq_force_integer_mv,
                    order_hint_bits,
                    enable_ref_frame_mvs,
                    enable_warped_motion,
                )
            };

            let (input, enable_superres) = trace_bool(input, ctx, "enable_superres")?;
            let (input, enable_cdef) = trace_bool(input, ctx, "enable_cdef")?;
            let (input, enable_restoration) = trace_bool(input, ctx, "enable_restoration")?;
            let (input, color_config) = color_config(input, ctx, seq_profile)?;
            let fgp_bit_offset = input.1;
            let (input, film_grain_params_present) =
                trace_bool(input, ctx, "film_grain_params_present")?;

            if WRITE {
                // Toggle the film grain params present flag
                // based on whether we are adding or removing film grain.
                // We use the bit offset captured BEFORE parsing the flag so
                // it points at the flag itself rather than one past it.
                obu_out
                    .last_mut()
                    .unwrap()
                    .set_bit(7 - fgp_bit_offset, self.incoming_grain_header.is_some());
                self.packet_out.extend_from_slice(&obu_out);
                debug!(
                    "Writing updated sequence header of size {} to packet_out, total packet size \
                     at {}",
                    obu_out.len(),
                    self.packet_out.len()
                );
                trace!("Packet contents: {}", to_binary_string(&obu_out));
            }

            Ok((
                input,
                SequenceHeader {
                    reduced_still_picture_header,
                    frame_id_numbers_present,
                    additional_frame_id_len_minus_1,
                    delta_frame_id_len_minus_2,
                    film_grain_params_present,
                    new_film_grain_state: self.incoming_grain_header.is_some(),
                    force_screen_content_tools,
                    force_integer_mv,
                    order_hint_bits,
                    frame_width_bits_minus_1,
                    frame_height_bits_minus_1,
                    max_frame_width_minus_1,
                    max_frame_height_minus_1,
                    decoder_model_info,
                    decoder_model_present_for_op,
                    operating_points_cnt_minus_1,
                    operating_point_idc,
                    cur_operating_point_idc,
                    timing_info,
                    enable_ref_frame_mvs,
                    enable_warped_motion,
                    enable_superres,
                    enable_cdef,
                    enable_restoration,
                    use_128x128_superblock,
                    color_config,
                },
            ))
        })(input)
    }
}

/// Parses sequence-level `timing_info` and keeps the fields needed later.
///
/// RATIONALE: frame-header parsing only needs `equal_picture_interval` to know
/// whether `temporal_point_info` is present; other timing fields are consumed
/// only to keep bit parsing aligned.
fn timing_info<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
) -> IResult<BitInput<'a>, TimingInfo, Error<BitInput<'a>>> {
    let (input, _num_units_in_display_tick) =
        trace_take_u32(input, ctx, 32, "num_units_in_display_tick")?;
    let (input, _time_scale) = trace_take_u32(input, ctx, 32, "time_scale")?;
    let (input, equal_picture_interval) = trace_bool(input, ctx, "equal_picture_interval")?;
    let input = if equal_picture_interval {
        let pos = ctx.pos(input);
        let (input, value) = uvlc(input)?;
        let bits_consumed = ctx.pos(input) - pos;
        trace_field(
            pos,
            "num_ticks_per_picture_minus_1",
            bits_consumed,
            u64::from(value),
        );
        input
    } else {
        input
    };
    Ok((
        input,
        TimingInfo {
            equal_picture_interval,
        },
    ))
}

/// Parses sequence-level decoder model timing widths.
///
/// The returned lengths are reused when parsing per-operating-point and
/// per-frame timing fields.
fn decoder_model_info<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
) -> IResult<BitInput<'a>, DecoderModelInfo, Error<BitInput<'a>>> {
    let (input, buffer_delay_length_minus_1) =
        trace_take_u8(input, ctx, 5, "buffer_delay_length_minus_1")?;
    let (input, _num_units_in_decoding_tick) =
        trace_take_u32(input, ctx, 32, "num_units_in_decoding_tick")?;
    let (input, buffer_removal_time_length_minus_1) =
        trace_take_u8(input, ctx, 5, "buffer_removal_time_length_minus_1")?;
    let (input, frame_presentation_time_length_minus_1) =
        trace_take_u8(input, ctx, 5, "frame_presentation_time_length_minus_1")?;
    Ok((
        input,
        DecoderModelInfo {
            buffer_delay_length_minus_1,
            buffer_removal_time_length_minus_1,
            frame_presentation_time_length_minus_1,
        },
    ))
}

/// Parses per-operating-point decoder buffering parameters.
///
/// CONTRACT: this helper only advances the bitstream; the parsed values are
/// sequence-level timing side data that are not required by grain workflows.
fn operating_parameters_info<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
    buffer_delay_length: usize,
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
    let (input, _decoder_buffer_delay) =
        trace_take_u64(input, ctx, buffer_delay_length, "decoder_buffer_delay")?;
    let (input, _encoder_buffer_delay) =
        trace_take_u64(input, ctx, buffer_delay_length, "encoder_buffer_delay")?;
    let (input, _low_delay_mode_flag) = trace_bool(input, ctx, "low_delay_mode_flag")?;
    Ok((input, ()))
}

#[allow(clippy::too_many_lines)]
/// Parses AV1 `color_config` and normalizes chroma layout metadata.
///
/// ASSUMPTION: enum-coded color fields are spec-valid; invalid values currently
/// cause a panic via `unwrap()` because this parser treats malformed bitstreams
/// as unrecoverable.
fn color_config<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
    seq_profile: u8,
) -> IResult<BitInput<'a>, ColorConfig, Error<BitInput<'a>>> {
    let bit_depth: u8;
    let (input, high_bitdepth) = trace_bool(input, ctx, "high_bitdepth")?;
    let input = if seq_profile == 2 && high_bitdepth {
        let (input, twelve_bit) = trace_bool(input, ctx, "twelve_bit")?;
        bit_depth = if twelve_bit { 12 } else { 10 };
        input
    } else {
        bit_depth = if high_bitdepth { 10 } else { 8 };
        input
    };
    let (input, monochrome) = if seq_profile == 1 {
        (input, false)
    } else {
        trace_bool(input, ctx, "mono_chrome")?
    };
    let num_planes = if monochrome { 1 } else { 3 };
    let (input, color_description_present_flag) =
        trace_bool(input, ctx, "color_description_present_flag")?;
    let (input, (color_primaries, transfer_characteristics, matrix_coefficients)) =
        if color_description_present_flag {
            let (input, color_primaries) = trace_take_u8(input, ctx, 8, "color_primaries")?;
            let (input, transfer_characteristics) =
                trace_take_u8(input, ctx, 8, "transfer_characteristics")?;
            let (input, matrix_coefficients) = trace_take_u8(input, ctx, 8, "matrix_coefficients")?;
            (
                input,
                (
                    ColorPrimaries::try_from(color_primaries).unwrap(),
                    TransferCharacteristics::try_from(transfer_characteristics).unwrap(),
                    MatrixCoefficients::try_from(matrix_coefficients).unwrap(),
                ),
            )
        } else {
            (
                input,
                (
                    ColorPrimaries::Unspecified,
                    TransferCharacteristics::Unspecified,
                    MatrixCoefficients::Unspecified,
                ),
            )
        };
    let (input, color_range, subsampling) = if monochrome {
        let (input, color_range) = trace_take_u8(input, ctx, 1, "color_range")?;
        return Ok((
            input,
            ColorConfig {
                color_primaries,
                transfer_characteristics,
                matrix_coefficients,
                color_range: ColorRange::try_from(color_range).unwrap(),
                num_planes,
                separate_uv_delta_q: false,
                subsampling: (1, 1),
            },
        ));
    } else if color_primaries == ColorPrimaries::Bt709
        && transfer_characteristics == TransferCharacteristics::Srgb
        && matrix_coefficients == MatrixCoefficients::Identity
    {
        (input, ColorRange::Full, (0, 0))
    } else {
        let (input, color_range) = trace_take_u8(input, ctx, 1, "color_range")?;
        let (input, ss_x, ss_y) = if seq_profile == 0 {
            (input, 1, 1)
        } else if seq_profile == 1 {
            (input, 0, 0)
        } else if bit_depth == 12 {
            let (input, ss_x) = trace_take_u8(input, ctx, 1, "subsampling_x")?;
            let (input, ss_y): (_, u8) = if ss_x > 0 {
                trace_take_u8(input, ctx, 1, "subsampling_y")?
            } else {
                (input, 0)
            };
            (input, ss_x, ss_y)
        } else {
            (input, 1, 0)
        };
        let input = if ss_x > 0 && ss_y > 0 {
            let (input, _chroma_sample_position) =
                trace_take_u8(input, ctx, 2, "chroma_sample_position")?;
            input
        } else {
            input
        };
        (
            input,
            ColorRange::try_from(color_range).unwrap(),
            (ss_x, ss_y),
        )
    };
    let (input, separate_uv_delta_q) = trace_bool(input, ctx, "separate_uv_delta_q")?;
    Ok((
        input,
        ColorConfig {
            color_primaries,
            transfer_characteristics,
            matrix_coefficients,
            color_range,
            num_planes,
            separate_uv_delta_q,
            subsampling,
        },
    ))
}

#[must_use]
/// Selects the active sequence operating point for downstream parsing.
///
/// COMPAT: the current CLI does not expose operating-point selection, so we
/// conservatively parse against operating point 0.
const fn choose_operating_point() -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::{
        super::trace::TraceCtx, super::util::BitInput, BitstreamParser, ColorPrimaries, ColorRange,
        MatrixCoefficients, SELECT_INTEGER_MV, SELECT_SCREEN_CONTENT_TOOLS,
        TransferCharacteristics, color_config, decoder_model_info, operating_parameters_info,
        timing_info,
    };
    use crate::GrainTableSegment;

    /// Creates a dummy `TraceCtx` for testing sub-functions in isolation.
    fn test_ctx(input: BitInput) -> TraceCtx {
        TraceCtx::new(input, 0)
    }

    #[derive(Default)]
    struct BitBuilder {
        bits: Vec<bool>,
    }

    impl BitBuilder {
        fn push_bool(&mut self, bit: bool) {
            self.bits.push(bit);
        }

        fn push_bits(&mut self, value: u64, width: usize) {
            for shift in (0..width).rev() {
                self.bits.push(((value >> shift) & 1) == 1);
            }
        }

        fn len(&self) -> usize {
            self.bits.len()
        }

        fn into_bytes(self) -> Vec<u8> {
            let mut bytes = vec![0u8; self.bits.len().div_ceil(8)];
            for (idx, bit) in self.bits.into_iter().enumerate() {
                if bit {
                    bytes[idx / 8] |= 1u8 << (7 - (idx % 8));
                }
            }
            bytes
        }
    }

    fn with_trailer(mut bits: BitBuilder) -> (Vec<u8>, usize) {
        let consumed_bits = bits.len();
        bits.push_bits(0b1010_0101, 8);
        (bits.into_bytes(), consumed_bits)
    }

    fn assert_remaining_position(remaining: (&[u8], usize), input: &[u8], consumed_bits: usize) {
        assert_eq!(remaining.0, &input[consumed_bits / 8..]);
        assert_eq!(remaining.1, consumed_bits % 8);
    }

    #[test]
    fn timing_info_parses_when_equal_picture_interval_is_false() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0x1122_3344, 32);
        bits.push_bits(0x5566_7788, 32);
        bits.push_bool(false);

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) =
            timing_info(input, test_ctx(input)).expect("expected timing_info without uvlc payload");

        assert!(!parsed.equal_picture_interval);
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn timing_info_parses_when_equal_picture_interval_is_true() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0x0102_0304, 32);
        bits.push_bits(0x0506_0708, 32);
        bits.push_bool(true);
        // UVLC with 3 leading zeros and payload 0b101.
        bits.push_bits(0b000_1101, 7);

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) =
            timing_info(input, test_ctx(input)).expect("expected timing_info with uvlc payload");

        assert!(parsed.equal_picture_interval);
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn timing_info_returns_error_when_uvlc_payload_is_truncated() {
        let mut bits = BitBuilder::default();
        // Start parsing at bit offset 3 so there is no zero-padding slack at the tail.
        bits.push_bits(0b101, 3);
        bits.push_bits(0x0102_0304, 32);
        bits.push_bits(0x0506_0708, 32);
        bits.push_bool(true);
        // Start a UVLC code but omit payload bits after the terminator.
        bits.push_bits(0b0001, 4);

        let data = bits.into_bytes();
        let input: BitInput = (&data, 3);
        assert!(timing_info(input, test_ctx(input)).is_err());
    }

    #[test]
    fn decoder_model_info_parses_all_fields() {
        let mut bits = BitBuilder::default();
        bits.push_bits(17, 5);
        bits.push_bits(0x1234_5678, 32);
        bits.push_bits(7, 5);
        bits.push_bits(31, 5);

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) = decoder_model_info(input, test_ctx(input))
            .expect("expected decoder_model_info to parse");

        assert_eq!(parsed.buffer_delay_length_minus_1, 17);
        assert_eq!(parsed.buffer_removal_time_length_minus_1, 7);
        assert_eq!(parsed.frame_presentation_time_length_minus_1, 31);
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn decoder_model_info_returns_error_when_input_is_too_short() {
        let data = [0u8; 5];
        let input: BitInput = (&data, 0);
        assert!(decoder_model_info(input, test_ctx(input)).is_err());
    }

    #[test]
    fn operating_parameters_info_parses_with_non_zero_buffer_delay_length() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0b1_0101, 5);
        bits.push_bits(0b0_0110, 5);
        bits.push_bool(true);

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, ()) = operating_parameters_info(input, test_ctx(input), 5)
            .expect("expected operating_parameters_info to parse");

        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn operating_parameters_info_supports_zero_buffer_delay_length() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false);

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, ()) = operating_parameters_info(input, test_ctx(input), 0)
            .expect("expected operating_parameters_info to parse with zero-width delays");

        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn operating_parameters_info_returns_error_when_low_delay_flag_is_missing() {
        let data = [0u8; 1];
        let input: BitInput = (&data, 2);
        assert!(operating_parameters_info(input, test_ctx(input), 3).is_err());
    }

    #[test]
    fn color_config_parses_monochrome_and_returns_early() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // high_bitdepth
        bits.push_bool(true); // twelve_bit (seq_profile == 2)
        bits.push_bool(true); // monochrome
        bits.push_bool(false); // color_description_present_flag
        bits.push_bool(false); // color_range

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) =
            color_config(input, test_ctx(input), 2).expect("expected monochrome color_config");

        assert_eq!(parsed.color_primaries, ColorPrimaries::Unspecified);
        assert_eq!(
            parsed.transfer_characteristics,
            TransferCharacteristics::Unspecified
        );
        assert_eq!(parsed.matrix_coefficients, MatrixCoefficients::Unspecified);
        assert_eq!(parsed.color_range, ColorRange::Limited);
        assert_eq!(parsed.num_planes, 1);
        assert!(!parsed.separate_uv_delta_q);
        assert_eq!(parsed.subsampling, (1, 1));
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn color_config_uses_srgb_identity_shortcut() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // high_bitdepth
        bits.push_bool(false); // twelve_bit (seq_profile == 2)
        bits.push_bool(false); // monochrome
        bits.push_bool(true); // color_description_present_flag
        bits.push_bits(1, 8); // color_primaries = Bt709
        bits.push_bits(13, 8); // transfer_characteristics = Srgb
        bits.push_bits(0, 8); // matrix_coefficients = Identity
        bits.push_bool(true); // separate_uv_delta_q

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) =
            color_config(input, test_ctx(input), 2).expect("expected srgb identity color_config");

        assert_eq!(parsed.color_primaries, ColorPrimaries::Bt709);
        assert_eq!(
            parsed.transfer_characteristics,
            TransferCharacteristics::Srgb
        );
        assert_eq!(parsed.matrix_coefficients, MatrixCoefficients::Identity);
        assert_eq!(parsed.color_range, ColorRange::Full);
        assert_eq!(parsed.subsampling, (0, 0));
        assert_eq!(parsed.num_planes, 3);
        assert!(parsed.separate_uv_delta_q);
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn color_config_profile0_reads_chroma_sample_position() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // high_bitdepth
        bits.push_bool(false); // monochrome
        bits.push_bool(false); // color_description_present_flag
        bits.push_bool(true); // color_range
        bits.push_bits(0b10, 2); // chroma_sample_position
        bits.push_bool(false); // separate_uv_delta_q

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) =
            color_config(input, test_ctx(input), 0).expect("expected profile 0 color_config");

        assert_eq!(parsed.color_primaries, ColorPrimaries::Unspecified);
        assert_eq!(
            parsed.transfer_characteristics,
            TransferCharacteristics::Unspecified
        );
        assert_eq!(parsed.matrix_coefficients, MatrixCoefficients::Unspecified);
        assert_eq!(parsed.color_range, ColorRange::Full);
        assert_eq!(parsed.subsampling, (1, 1));
        assert_eq!(parsed.num_planes, 3);
        assert!(!parsed.separate_uv_delta_q);
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn color_config_profile1_forces_non_monochrome_and_444_subsampling() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // high_bitdepth
        bits.push_bool(false); // color_description_present_flag
        bits.push_bool(true); // color_range
        bits.push_bool(false); // separate_uv_delta_q

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) =
            color_config(input, test_ctx(input), 1).expect("expected profile 1 color_config");

        assert_eq!(parsed.color_range, ColorRange::Full);
        assert_eq!(parsed.num_planes, 3);
        assert_eq!(parsed.subsampling, (0, 0));
        assert!(!parsed.separate_uv_delta_q);
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn color_config_profile2_twelve_bit_skips_ss_y_when_ss_x_is_zero() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // high_bitdepth
        bits.push_bool(true); // twelve_bit
        bits.push_bool(false); // monochrome
        bits.push_bool(false); // color_description_present_flag
        bits.push_bool(false); // color_range
        bits.push_bool(false); // ss_x
        bits.push_bool(true); // separate_uv_delta_q

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) = color_config(input, test_ctx(input), 2)
            .expect("expected profile 2 12-bit ss_x=0 color_config");

        assert_eq!(parsed.color_range, ColorRange::Limited);
        assert_eq!(parsed.subsampling, (0, 0));
        assert!(parsed.separate_uv_delta_q);
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn color_config_profile2_twelve_bit_reads_ss_y_and_chroma_position() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // high_bitdepth
        bits.push_bool(true); // twelve_bit
        bits.push_bool(false); // monochrome
        bits.push_bool(false); // color_description_present_flag
        bits.push_bool(true); // color_range
        bits.push_bool(true); // ss_x
        bits.push_bool(true); // ss_y
        bits.push_bits(0b01, 2); // chroma_sample_position
        bits.push_bool(false); // separate_uv_delta_q

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) = color_config(input, test_ctx(input), 2)
            .expect("expected profile 2 12-bit ss_x=1/ss_y=1 color_config");

        assert_eq!(parsed.color_range, ColorRange::Full);
        assert_eq!(parsed.subsampling, (1, 1));
        assert!(!parsed.separate_uv_delta_q);
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn color_config_profile2_ten_bit_uses_default_subsampling() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // high_bitdepth
        bits.push_bool(false); // twelve_bit => 10-bit
        bits.push_bool(false); // monochrome
        bits.push_bool(false); // color_description_present_flag
        bits.push_bool(false); // color_range
        bits.push_bool(true); // separate_uv_delta_q

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) = color_config(input, test_ctx(input), 2)
            .expect("expected profile 2 10-bit color_config");

        assert_eq!(parsed.color_range, ColorRange::Limited);
        assert_eq!(parsed.subsampling, (1, 0));
        assert!(parsed.separate_uv_delta_q);
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn color_config_returns_error_when_color_description_triplet_is_truncated() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // high_bitdepth
        bits.push_bool(false); // monochrome
        bits.push_bool(true); // color_description_present_flag
        bits.push_bits(1, 8); // partial triplet: only color_primaries

        let data = bits.into_bytes();
        let input: BitInput = (&data, 0);
        assert!(color_config(input, test_ctx(input), 0).is_err());
    }

    #[test]
    #[should_panic]
    fn color_config_panics_when_color_primaries_code_is_invalid() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // high_bitdepth
        bits.push_bool(false); // monochrome
        bits.push_bool(true); // color_description_present_flag
        bits.push_bits(3, 8); // invalid color_primaries enum value
        bits.push_bits(1, 8); // transfer_characteristics
        bits.push_bits(1, 8); // matrix_coefficients

        let data = bits.into_bytes();
        let input: BitInput = (&data, 0);
        _ = color_config(input, test_ctx(input), 0);
    }

    // --- parse_sequence_header helpers ---

    fn make_parser<const WRITE: bool>(
        size: usize,
        incoming_grain_header: Option<Vec<GrainTableSegment>>,
    ) -> BitstreamParser<WRITE> {
        BitstreamParser {
            reader: None,
            writer: None,
            packet_out: Vec::new(),
            incoming_grain_header,
            parsed: false,
            size,
            seen_frame_header: false,
            sequence_header: None,
            previous_frame_header: None,
            ref_frame_idx: Default::default(),
            ref_order_hint: Default::default(),
            big_ref_order_hint: Default::default(),
            big_ref_valid: Default::default(),
            big_order_hints: Default::default(),
            grain_headers: Vec::new(),
            strict_mode: false,
        }
    }

    /// Appends a minimal profile-0 8-bit color_config (7 bits).
    fn push_color_config_profile0_8bit(bits: &mut BitBuilder) {
        bits.push_bool(false); // high_bitdepth → 8-bit
        bits.push_bool(false); // monochrome (profile 0 reads this bit)
        bits.push_bool(false); // color_description_present_flag
        bits.push_bool(false); // color_range = limited
        bits.push_bits(0, 2); // chroma_sample_position (profile 0: ss_x=1, ss_y=1)
        bits.push_bool(false); // separate_uv_delta_q
    }

    /// Appends the reduced-path tail: superblock flags through film_grain.
    fn push_reduced_suffix(bits: &mut BitBuilder, film_grain: bool) {
        bits.push_bool(false); // use_128x128_superblock
        bits.push_bool(false); // enable_filter_intra
        bits.push_bool(false); // enable_intra_edge_filter
        // Reduced: force_screen_content_tools=SELECT, force_integer_mv=SELECT,
        // order_hint_bits=0, enable_ref_frame_mvs=false, enable_warped_motion=false
        // (no bits consumed for capability fields)
        bits.push_bool(false); // enable_superres
        bits.push_bool(false); // enable_cdef
        bits.push_bool(false); // enable_restoration
        push_color_config_profile0_8bit(bits);
        bits.push_bool(film_grain);
    }

    /// Appends the non-reduced tail from use_128x128_superblock through
    /// film_grain with the simplest branch choices: no order hint,
    /// choose_screen_content_tools=true, choose_integer_mv=true.
    fn push_minimal_non_reduced_suffix(bits: &mut BitBuilder, film_grain: bool) {
        bits.push_bool(false); // use_128x128_superblock
        bits.push_bool(false); // enable_filter_intra
        bits.push_bool(false); // enable_intra_edge_filter
        bits.push_bool(false); // enable_interintra_compound
        bits.push_bool(false); // enable_masked_compound
        bits.push_bool(false); // enable_warped_motion
        bits.push_bool(false); // enable_dual_filter
        bits.push_bool(false); // enable_order_hint
        bits.push_bool(true); // seq_choose_screen_content_tools → SELECT
        bits.push_bool(true); // seq_choose_integer_mv → SELECT
        bits.push_bool(false); // enable_superres
        bits.push_bool(false); // enable_cdef
        bits.push_bool(false); // enable_restoration
        push_color_config_profile0_8bit(bits);
        bits.push_bool(film_grain);
    }

    // --- parse_sequence_header read-mode tests ---

    #[test]
    fn reduced_still_picture_header_sets_defaults() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 3); // seq_profile = 0
        bits.push_bool(true); // still_picture
        bits.push_bool(true); // reduced_still_picture_header
        bits.push_bits(4, 5); // seq_level_idx = 4
        bits.push_bits(0, 4); // frame_width_bits_minus_1 = 0
        bits.push_bits(0, 4); // frame_height_bits_minus_1 = 0
        bits.push_bits(0, 1); // max_frame_width_minus_1 = 0
        bits.push_bits(0, 1); // max_frame_height_minus_1 = 0
        push_reduced_suffix(&mut bits, false);

        let data = bits.into_bytes();
        let mut parser = make_parser::<false>(0, None);
        let (_, seq) = parser
            .parse_sequence_header(&data, 0)
            .expect("reduced still picture header should parse");

        assert!(seq.reduced_still_picture_header);
        assert!(!seq.frame_id_numbers_present);
        assert_eq!(seq.delta_frame_id_len_minus_2, 0);
        assert_eq!(seq.additional_frame_id_len_minus_1, 0);
        assert_eq!(seq.force_screen_content_tools, SELECT_SCREEN_CONTENT_TOOLS);
        assert_eq!(seq.force_integer_mv, SELECT_INTEGER_MV);
        assert_eq!(seq.order_hint_bits, 0);
        assert!(!seq.enable_order_hint());
        assert!(!seq.enable_ref_frame_mvs);
        assert!(!seq.enable_warped_motion);
        assert!(!seq.enable_superres);
        assert!(!seq.enable_cdef);
        assert!(!seq.enable_restoration);
        assert!(!seq.film_grain_params_present);
        assert!(!seq.new_film_grain_state);
        assert!(seq.timing_info.is_none());
        assert!(seq.decoder_model_info.is_none());
        assert_eq!(seq.operating_points_cnt_minus_1, 0);
        assert_eq!(seq.frame_width_bits_minus_1, 0);
        assert_eq!(seq.frame_height_bits_minus_1, 0);
        assert_eq!(seq.max_frame_width_minus_1, 0);
        assert_eq!(seq.max_frame_height_minus_1, 0);
    }

    #[test]
    fn non_reduced_no_timing_info_single_op() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 3); // seq_profile = 0
        bits.push_bool(false); // still_picture
        bits.push_bool(false); // reduced_still_picture_header
        bits.push_bool(false); // timing_info_present_flag
        bits.push_bool(false); // initial_display_delay_present_flag
        bits.push_bits(0, 5); // operating_points_cnt_minus_1 = 0
        bits.push_bits(0, 12); // operating_point_idc[0] = 0
        bits.push_bits(4, 5); // seq_level_idx = 4 (≤7, no tier)
        bits.push_bits(3, 4); // frame_width_bits_minus_1 = 3
        bits.push_bits(2, 4); // frame_height_bits_minus_1 = 2
        bits.push_bits(10, 4); // max_frame_width_minus_1 = 10 (4 bits)
        bits.push_bits(5, 3); // max_frame_height_minus_1 = 5 (3 bits)
        bits.push_bool(false); // frame_id_numbers_present
        push_minimal_non_reduced_suffix(&mut bits, false);

        let data = bits.into_bytes();
        let mut parser = make_parser::<false>(0, None);
        let (_, seq) = parser
            .parse_sequence_header(&data, 0)
            .expect("non-reduced single op should parse");

        assert!(!seq.reduced_still_picture_header);
        assert!(seq.timing_info.is_none());
        assert!(seq.decoder_model_info.is_none());
        assert_eq!(seq.operating_points_cnt_minus_1, 0);
        assert_eq!(seq.operating_point_idc.as_slice(), &[0]);
        assert!(!seq.frame_id_numbers_present);
        assert_eq!(seq.force_screen_content_tools, SELECT_SCREEN_CONTENT_TOOLS);
        assert_eq!(seq.force_integer_mv, SELECT_INTEGER_MV);
        assert_eq!(seq.order_hint_bits, 0);
        assert!(!seq.enable_ref_frame_mvs);
        assert!(!seq.enable_warped_motion);
        assert_eq!(seq.frame_width_bits_minus_1, 3);
        assert_eq!(seq.frame_height_bits_minus_1, 2);
        assert_eq!(seq.max_frame_width_minus_1, 10);
        assert_eq!(seq.max_frame_height_minus_1, 5);
        assert!(!seq.film_grain_params_present);
    }

    #[test]
    fn non_reduced_with_timing_info_no_decoder_model() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 3); // seq_profile = 0
        bits.push_bool(false); // still_picture
        bits.push_bool(false); // reduced
        bits.push_bool(true); // timing_info_present_flag
        // timing_info:
        bits.push_bits(1000, 32); // num_units_in_display_tick
        bits.push_bits(24000, 32); // time_scale
        bits.push_bool(false); // equal_picture_interval
        bits.push_bool(false); // decoder_model_present_flag
        bits.push_bool(false); // initial_display_delay_present_flag
        bits.push_bits(0, 5); // operating_points_cnt_minus_1 = 0
        bits.push_bits(0, 12); // operating_point_idc[0]
        bits.push_bits(4, 5); // seq_level_idx = 4
        bits.push_bits(0, 4); // frame_width_bits_minus_1 = 0
        bits.push_bits(0, 4); // frame_height_bits_minus_1 = 0
        bits.push_bits(0, 1); // max_frame_width_minus_1 = 0
        bits.push_bits(0, 1); // max_frame_height_minus_1 = 0
        bits.push_bool(false); // frame_id_numbers_present
        push_minimal_non_reduced_suffix(&mut bits, false);

        let data = bits.into_bytes();
        let mut parser = make_parser::<false>(0, None);
        let (_, seq) = parser
            .parse_sequence_header(&data, 0)
            .expect("timing info without decoder model should parse");

        let ti = seq.timing_info.expect("timing_info should be Some");
        assert!(!ti.equal_picture_interval);
        assert!(seq.decoder_model_info.is_none());
    }

    #[test]
    fn non_reduced_with_decoder_model_and_multi_op() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 3); // seq_profile = 0
        bits.push_bool(false); // still_picture
        bits.push_bool(false); // reduced
        bits.push_bool(true); // timing_info_present_flag
        // timing_info:
        bits.push_bits(0, 32); // num_units_in_display_tick
        bits.push_bits(0, 32); // time_scale
        bits.push_bool(false); // equal_picture_interval
        bits.push_bool(true); // decoder_model_present_flag
        // decoder_model_info:
        bits.push_bits(0, 5); // buffer_delay_length_minus_1 = 0
        bits.push_bits(0, 32); // num_units_in_decoding_tick
        bits.push_bits(0, 5); // buffer_removal_time_length_minus_1 = 0
        bits.push_bits(0, 5); // frame_presentation_time_length_minus_1 = 0
        bits.push_bool(true); // initial_display_delay_present_flag
        bits.push_bits(1, 5); // operating_points_cnt_minus_1 = 1
        // --- op 0: seq_level_idx=8 (>7), tier read, decoder model present
        bits.push_bits(0, 12); // operating_point_idc[0] = 0
        bits.push_bits(8, 5); // seq_level_idx = 8 (>7)
        bits.push_bool(false); // seq_tier
        bits.push_bool(true); // decoder_model_present_for_op[0]
        // operating_parameters_info (buffer_delay_length=1):
        bits.push_bits(0, 1); // decoder_buffer_delay
        bits.push_bits(0, 1); // encoder_buffer_delay
        bits.push_bool(false); // low_delay_mode_flag
        bits.push_bool(true); // initial_display_delay_present_for_op[0]
        bits.push_bits(5, 4); // initial_display_delay_minus_1 = 5
        // --- op 1: seq_level_idx=4 (≤7), no tier, no decoder model
        bits.push_bits(0x100, 12); // operating_point_idc[1] = 0x100
        bits.push_bits(4, 5); // seq_level_idx = 4 (≤7, no tier)
        bits.push_bool(false); // decoder_model_present_for_op[1]
        bits.push_bool(false); // initial_display_delay_present_for_op[1]
        // --- dimensions
        bits.push_bits(0, 4); // frame_width_bits_minus_1 = 0
        bits.push_bits(0, 4); // frame_height_bits_minus_1 = 0
        bits.push_bits(0, 1); // max_frame_width_minus_1
        bits.push_bits(0, 1); // max_frame_height_minus_1
        bits.push_bool(false); // frame_id_numbers_present
        push_minimal_non_reduced_suffix(&mut bits, false);

        let data = bits.into_bytes();
        let mut parser = make_parser::<false>(0, None);
        let (_, seq) = parser
            .parse_sequence_header(&data, 0)
            .expect("decoder model with multi-op should parse");

        let ti = seq.timing_info.expect("timing_info should be Some");
        assert!(!ti.equal_picture_interval);
        let dmi = seq
            .decoder_model_info
            .expect("decoder_model_info should be Some");
        assert_eq!(dmi.buffer_delay_length_minus_1, 0);
        assert_eq!(dmi.buffer_removal_time_length_minus_1, 0);
        assert_eq!(dmi.frame_presentation_time_length_minus_1, 0);
        assert_eq!(seq.operating_points_cnt_minus_1, 1);
        assert_eq!(seq.operating_point_idc.as_slice(), &[0, 0x100]);
        assert_eq!(seq.decoder_model_present_for_op.as_slice(), &[true, false]);
    }

    #[test]
    fn frame_id_numbers_present_parses_lengths() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 3); // seq_profile = 0
        bits.push_bool(false); // still_picture
        bits.push_bool(false); // reduced
        bits.push_bool(false); // timing_info_present
        bits.push_bool(false); // initial_display_delay_present
        bits.push_bits(0, 5); // operating_points_cnt_minus_1 = 0
        bits.push_bits(0, 12); // operating_point_idc[0]
        bits.push_bits(4, 5); // seq_level_idx = 4
        bits.push_bits(0, 4); // frame_width_bits_minus_1 = 0
        bits.push_bits(0, 4); // frame_height_bits_minus_1 = 0
        bits.push_bits(0, 1); // max_frame_width_minus_1
        bits.push_bits(0, 1); // max_frame_height_minus_1
        bits.push_bool(true); // frame_id_numbers_present
        bits.push_bits(5, 4); // delta_frame_id_len_minus_2 = 5
        bits.push_bits(3, 3); // additional_frame_id_len_minus_1 = 3
        push_minimal_non_reduced_suffix(&mut bits, false);

        let data = bits.into_bytes();
        let mut parser = make_parser::<false>(0, None);
        let (_, seq) = parser
            .parse_sequence_header(&data, 0)
            .expect("frame id numbers present should parse");

        assert!(seq.frame_id_numbers_present);
        assert_eq!(seq.delta_frame_id_len_minus_2, 5);
        assert_eq!(seq.additional_frame_id_len_minus_1, 3);
    }

    #[test]
    fn enable_order_hint_parses_ref_frame_mvs_and_bits() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 3); // seq_profile = 0
        bits.push_bool(false); // still_picture
        bits.push_bool(false); // reduced
        bits.push_bool(false); // timing_info_present
        bits.push_bool(false); // initial_display_delay_present
        bits.push_bits(0, 5); // operating_points_cnt_minus_1 = 0
        bits.push_bits(0, 12); // operating_point_idc[0]
        bits.push_bits(4, 5); // seq_level_idx = 4
        bits.push_bits(0, 4); // frame_width_bits_minus_1 = 0
        bits.push_bits(0, 4); // frame_height_bits_minus_1 = 0
        bits.push_bits(0, 1); // max_frame_width_minus_1
        bits.push_bits(0, 1); // max_frame_height_minus_1
        bits.push_bool(false); // frame_id_numbers_present
        bits.push_bool(false); // use_128x128_superblock
        bits.push_bool(false); // enable_filter_intra
        bits.push_bool(false); // enable_intra_edge_filter
        bits.push_bool(false); // enable_interintra_compound
        bits.push_bool(false); // enable_masked_compound
        bits.push_bool(true); // enable_warped_motion
        bits.push_bool(false); // enable_dual_filter
        bits.push_bool(true); // enable_order_hint
        bits.push_bool(false); // enable_jnt_comp
        bits.push_bool(true); // enable_ref_frame_mvs
        bits.push_bool(true); // seq_choose_screen_content_tools → SELECT
        bits.push_bool(true); // seq_choose_integer_mv → SELECT
        bits.push_bits(5, 3); // order_hint_bits_minus_1 = 5 → order_hint_bits = 6
        bits.push_bool(false); // enable_superres
        bits.push_bool(false); // enable_cdef
        bits.push_bool(false); // enable_restoration
        push_color_config_profile0_8bit(&mut bits);
        bits.push_bool(false); // film_grain_params_present

        let data = bits.into_bytes();
        let mut parser = make_parser::<false>(0, None);
        let (_, seq) = parser
            .parse_sequence_header(&data, 0)
            .expect("order hint with ref_frame_mvs should parse");

        assert!(seq.enable_order_hint());
        assert_eq!(seq.order_hint_bits, 6);
        assert!(seq.enable_ref_frame_mvs);
        assert!(seq.enable_warped_motion);
        assert_eq!(seq.force_screen_content_tools, SELECT_SCREEN_CONTENT_TOOLS);
        assert_eq!(seq.force_integer_mv, SELECT_INTEGER_MV);
    }

    #[test]
    fn explicit_screen_content_tools_and_integer_mv() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 3); // seq_profile = 0
        bits.push_bool(false); // still_picture
        bits.push_bool(false); // reduced
        bits.push_bool(false); // timing_info_present
        bits.push_bool(false); // initial_display_delay_present
        bits.push_bits(0, 5); // operating_points_cnt_minus_1 = 0
        bits.push_bits(0, 12); // operating_point_idc[0]
        bits.push_bits(4, 5); // seq_level_idx = 4
        bits.push_bits(0, 4); // frame_width_bits_minus_1 = 0
        bits.push_bits(0, 4); // frame_height_bits_minus_1 = 0
        bits.push_bits(0, 1); // max_frame_width_minus_1
        bits.push_bits(0, 1); // max_frame_height_minus_1
        bits.push_bool(false); // frame_id_numbers_present
        bits.push_bool(false); // use_128x128_superblock
        bits.push_bool(false); // enable_filter_intra
        bits.push_bool(false); // enable_intra_edge_filter
        bits.push_bool(false); // enable_interintra_compound
        bits.push_bool(false); // enable_masked_compound
        bits.push_bool(false); // enable_warped_motion
        bits.push_bool(false); // enable_dual_filter
        bits.push_bool(false); // enable_order_hint
        bits.push_bool(false); // seq_choose_screen_content_tools
        bits.push_bits(1, 1); // seq_force_screen_content_tools = 1
        bits.push_bool(false); // seq_choose_integer_mv
        bits.push_bits(0, 1); // seq_force_integer_mv = 0
        bits.push_bool(false); // enable_superres
        bits.push_bool(false); // enable_cdef
        bits.push_bool(false); // enable_restoration
        push_color_config_profile0_8bit(&mut bits);
        bits.push_bool(false); // film_grain_params_present

        let data = bits.into_bytes();
        let mut parser = make_parser::<false>(0, None);
        let (_, seq) = parser
            .parse_sequence_header(&data, 0)
            .expect("explicit screen content tools and integer mv should parse");

        assert_eq!(seq.force_screen_content_tools, 1);
        assert_eq!(seq.force_integer_mv, 0);
    }

    #[test]
    fn zero_screen_content_tools_forces_select_integer_mv() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 3); // seq_profile = 0
        bits.push_bool(false); // still_picture
        bits.push_bool(false); // reduced
        bits.push_bool(false); // timing_info_present
        bits.push_bool(false); // initial_display_delay_present
        bits.push_bits(0, 5); // operating_points_cnt_minus_1 = 0
        bits.push_bits(0, 12); // operating_point_idc[0]
        bits.push_bits(4, 5); // seq_level_idx = 4
        bits.push_bits(0, 4); // frame_width_bits_minus_1 = 0
        bits.push_bits(0, 4); // frame_height_bits_minus_1 = 0
        bits.push_bits(0, 1); // max_frame_width_minus_1
        bits.push_bits(0, 1); // max_frame_height_minus_1
        bits.push_bool(false); // frame_id_numbers_present
        bits.push_bool(false); // use_128x128_superblock
        bits.push_bool(false); // enable_filter_intra
        bits.push_bool(false); // enable_intra_edge_filter
        bits.push_bool(false); // enable_interintra_compound
        bits.push_bool(false); // enable_masked_compound
        bits.push_bool(false); // enable_warped_motion
        bits.push_bool(false); // enable_dual_filter
        bits.push_bool(false); // enable_order_hint
        bits.push_bool(false); // seq_choose_screen_content_tools
        bits.push_bits(0, 1); // seq_force_screen_content_tools = 0
        // force=0 → force_integer_mv=SELECT without reading bits
        bits.push_bool(false); // enable_superres
        bits.push_bool(false); // enable_cdef
        bits.push_bool(false); // enable_restoration
        push_color_config_profile0_8bit(&mut bits);
        bits.push_bool(false); // film_grain_params_present

        let data = bits.into_bytes();
        let mut parser = make_parser::<false>(0, None);
        let (_, seq) = parser
            .parse_sequence_header(&data, 0)
            .expect("zero screen content tools should force select integer mv");

        assert_eq!(seq.force_screen_content_tools, 0);
        assert_eq!(seq.force_integer_mv, SELECT_INTEGER_MV);
    }

    #[test]
    fn film_grain_params_present_true() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 3); // seq_profile = 0
        bits.push_bool(false); // still_picture
        bits.push_bool(false); // reduced
        bits.push_bool(false); // timing_info_present
        bits.push_bool(false); // initial_display_delay_present
        bits.push_bits(0, 5); // operating_points_cnt_minus_1 = 0
        bits.push_bits(0, 12); // operating_point_idc[0]
        bits.push_bits(4, 5); // seq_level_idx = 4
        bits.push_bits(0, 4); // frame_width_bits_minus_1 = 0
        bits.push_bits(0, 4); // frame_height_bits_minus_1 = 0
        bits.push_bits(0, 1); // max_frame_width_minus_1
        bits.push_bits(0, 1); // max_frame_height_minus_1
        bits.push_bool(false); // frame_id_numbers_present
        push_minimal_non_reduced_suffix(&mut bits, true);

        let data = bits.into_bytes();
        let mut parser = make_parser::<false>(0, None);
        let (_, seq) = parser
            .parse_sequence_header(&data, 0)
            .expect("film grain present should parse");

        assert!(seq.film_grain_params_present);
    }

    // --- parse_sequence_header write-mode tests ---

    /// Builds the minimal non-reduced sequence header bitstream used by
    /// WRITE mode tests. Returns `(data, size)` where `size` is the OBU
    /// payload length covering through the film_grain_params_present byte.
    fn build_write_test_bitstream(film_grain: bool) -> (Vec<u8>, usize) {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 3); // seq_profile = 0
        bits.push_bool(false); // still_picture
        bits.push_bool(false); // reduced
        bits.push_bool(false); // timing_info_present
        bits.push_bool(false); // initial_display_delay_present
        bits.push_bits(0, 5); // operating_points_cnt_minus_1 = 0
        bits.push_bits(0, 12); // operating_point_idc[0]
        bits.push_bits(4, 5); // seq_level_idx = 4
        bits.push_bits(0, 4); // frame_width_bits_minus_1 = 0
        bits.push_bits(0, 4); // frame_height_bits_minus_1 = 0
        bits.push_bits(0, 1); // max_frame_width_minus_1
        bits.push_bits(0, 1); // max_frame_height_minus_1
        bits.push_bool(false); // frame_id_numbers_present
        push_minimal_non_reduced_suffix(&mut bits, film_grain);
        let data = bits.into_bytes();
        let size = data.len();
        (data, size)
    }

    #[test]
    fn write_mode_sets_grain_bit_when_grain_present() {
        let (data, size) = build_write_test_bitstream(false);
        let mut parser = make_parser::<true>(size, Some(Vec::new()));
        let (_, seq) = parser
            .parse_sequence_header(&data, 0)
            .expect("write mode with grain present should parse");

        assert!(!seq.film_grain_params_present);
        assert!(seq.new_film_grain_state);
        assert_eq!(parser.packet_out.len(), size);
        // Film grain bit is at bit 60 → byte 7, bit 3 (0=LSB).
        assert_ne!(parser.packet_out[7] & (1 << 3), 0);
    }

    #[test]
    fn write_mode_clears_grain_bit_when_grain_absent() {
        let (data, size) = build_write_test_bitstream(true);
        let mut parser = make_parser::<true>(size, None);
        let (_, seq) = parser
            .parse_sequence_header(&data, 0)
            .expect("write mode without grain should parse");

        assert!(seq.film_grain_params_present);
        assert!(!seq.new_film_grain_state);
        assert_eq!(parser.packet_out.len(), size);
        // Film grain bit at byte 7, bit 3 should be cleared.
        assert_eq!(parser.packet_out[7] & (1 << 3), 0);
    }
}
