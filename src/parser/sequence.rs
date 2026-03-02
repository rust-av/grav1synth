use arrayvec::ArrayVec;
use bit::BitIndex;
use log::debug;
use nom::{IResult, bits::bits, bits::complete as bit_parsers, error::Error};
use num_enum::TryFromPrimitive;

use super::{
    BitstreamParser,
    util::{BitInput, take_bool_bit, uvlc},
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
    ) -> IResult<&'a [u8], SequenceHeader, Error<&'a [u8]>> {
        let mut obu_out = if WRITE {
            input[..self.size].to_owned()
        } else {
            Vec::new()
        };
        bits(move |input| {
            let (input, seq_profile): (_, u8) = bit_parsers::take(3usize)(input)?;
            let (input, _still_picture) = take_bool_bit(input)?;
            let (input, reduced_still_picture_header) = take_bool_bit(input)?;
            let (
                input,
                decoder_model_info,
                operating_points_cnt_minus_1,
                decoder_model_present_for_op,
                operating_point_idc,
                timing_info,
            ) = if reduced_still_picture_header {
                let (input, _seq_level_idx): (_, u8) = bit_parsers::take(5usize)(input)?;
                (input, None, 0, ArrayVec::new(), ArrayVec::new(), None)
            } else {
                let (input, timing_info_present_flag) = take_bool_bit(input)?;
                let (input, decoder_model_info, timing_info) = if timing_info_present_flag {
                    let (input, timing_info) = timing_info(input)?;
                    let (input, flag) = take_bool_bit(input)?;
                    let (input, decoder_model, timing_info) = if flag {
                        let (input, decoder_model) = decoder_model_info(input)?;
                        (input, Some(decoder_model), timing_info)
                    } else {
                        (input, None, timing_info)
                    };
                    (input, decoder_model, Some(timing_info))
                } else {
                    (input, None, None)
                };
                let (input, initial_display_delay_present_flag) = take_bool_bit(input)?;

                let mut decoder_model_present_for_op = ArrayVec::new();
                let mut operating_point_idc = ArrayVec::new();
                let (mut input, operating_points_cnt_minus_1): (_, usize) =
                    bit_parsers::take(5usize)(input)?;
                for _ in 0..=operating_points_cnt_minus_1 {
                    let inner_input = input;
                    let (inner_input, cur_operating_point_idc): (_, u16) =
                        bit_parsers::take(12usize)(inner_input)?;
                    operating_point_idc.push(cur_operating_point_idc);
                    let (inner_input, seq_level_idx): (_, u8) =
                        bit_parsers::take(5usize)(inner_input)?;
                    let (inner_input, _seq_tier) = if seq_level_idx > 7 {
                        take_bool_bit(inner_input)?
                    } else {
                        (inner_input, false)
                    };
                    let (inner_input, cur_decoder_model_present_for_op) =
                        if let Some(decoder_model_info) = decoder_model_info {
                            let (inner_input, flag) = take_bool_bit(inner_input)?;
                            if flag {
                                (
                                    operating_parameters_info(
                                        inner_input,
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
                            let (inner_input, flag) = take_bool_bit(inner_input)?;
                            if flag {
                                let (inner_input, _initial_display_delay_minus_1): (_, u8) =
                                    bit_parsers::take(4usize)(inner_input)?;
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
            let (input, frame_width_bits_minus_1) = bit_parsers::take(4usize)(input)?;
            let (input, frame_height_bits_minus_1) = bit_parsers::take(4usize)(input)?;
            let (input, max_frame_width_minus_1) =
                bit_parsers::take(frame_width_bits_minus_1 + 1)(input)?;
            let (input, max_frame_height_minus_1) =
                bit_parsers::take(frame_height_bits_minus_1 + 1)(input)?;
            let (input, frame_id_numbers_present) = if reduced_still_picture_header {
                (input, false)
            } else {
                take_bool_bit(input)?
            };
            let (input, delta_frame_id_len_minus_2, additional_frame_id_len_minus_1) =
                if frame_id_numbers_present {
                    let (input, delta_frame_id_len_minus_2) = bit_parsers::take(4usize)(input)?;
                    let (input, additional_frame_id_len_minus_1) =
                        bit_parsers::take(3usize)(input)?;
                    (
                        input,
                        delta_frame_id_len_minus_2,
                        additional_frame_id_len_minus_1,
                    )
                } else {
                    (input, 0, 0)
                };
            let (input, use_128x128_superblock) = take_bool_bit(input)?;
            let (input, _enable_filter_intra) = take_bool_bit(input)?;
            let (input, _enable_intra_edge_filter) = take_bool_bit(input)?;
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
                let (input, _enable_interintra_compound) = take_bool_bit(input)?;
                let (input, _enable_masked_compound) = take_bool_bit(input)?;
                let (input, enable_warped_motion) = take_bool_bit(input)?;
                let (input, _enable_dual_filter) = take_bool_bit(input)?;
                let (input, enable_order_hint) = take_bool_bit(input)?;
                let (input, enable_ref_frame_mvs) = if enable_order_hint {
                    let (input, _enable_jnt_comp) = take_bool_bit(input)?;
                    let (input, enable_ref_frame_mvs) = take_bool_bit(input)?;
                    (input, enable_ref_frame_mvs)
                } else {
                    (input, false)
                };
                let (input, seq_choose_screen_content_tools) = take_bool_bit(input)?;
                let (input, seq_force_screen_content_tools): (_, u8) =
                    if seq_choose_screen_content_tools {
                        (input, SELECT_SCREEN_CONTENT_TOOLS)
                    } else {
                        bit_parsers::take(1usize)(input)?
                    };

                let (input, seq_force_integer_mv) = if seq_force_screen_content_tools > 0 {
                    let (input, seq_choose_integer_mv) = take_bool_bit(input)?;
                    if seq_choose_integer_mv {
                        (input, SELECT_INTEGER_MV)
                    } else {
                        bit_parsers::take(1usize)(input)?
                    }
                } else {
                    (input, SELECT_INTEGER_MV)
                };
                let (input, order_hint_bits) = if enable_order_hint {
                    let (input, order_hint_bits_minus_1): (_, usize) =
                        bit_parsers::take(3usize)(input)?;
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

            let (input, enable_superres) = take_bool_bit(input)?;
            let (input, enable_cdef) = take_bool_bit(input)?;
            let (input, enable_restoration) = take_bool_bit(input)?;
            let (input, color_config) = color_config(input, seq_profile)?;

            if WRITE {
                // Toggle the film grain params present flag
                // based on whether we are adding or removing film grain.
                let bit_offset = input.1;
                obu_out
                    .last_mut()
                    .unwrap()
                    .set_bit(7 - bit_offset, self.incoming_grain_header.is_some());
                self.packet_out.extend_from_slice(&obu_out);
                debug!(
                    "Writing updated sequence header of size {} to packet_out, total packet size \
                     at {}",
                    obu_out.len(),
                    self.packet_out.len()
                );
            }

            let (input, film_grain_params_present) = take_bool_bit(input)?;

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
fn timing_info(input: BitInput) -> IResult<BitInput, TimingInfo, Error<BitInput>> {
    let (input, _num_units_in_display_tick): (_, u32) = bit_parsers::take(32usize)(input)?;
    let (input, _time_scale): (_, u32) = bit_parsers::take(32usize)(input)?;
    let (input, equal_picture_interval) = take_bool_bit(input)?;
    let input = if equal_picture_interval {
        let (input, _num_ticks_per_picture_minus_1) = uvlc(input)?;
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
fn decoder_model_info(input: BitInput) -> IResult<BitInput, DecoderModelInfo, Error<BitInput>> {
    let (input, buffer_delay_length_minus_1) = bit_parsers::take(5usize)(input)?;
    let (input, _num_units_in_decoding_tick): (_, u32) = bit_parsers::take(32usize)(input)?;
    let (input, buffer_removal_time_length_minus_1) = bit_parsers::take(5usize)(input)?;
    let (input, frame_presentation_time_length_minus_1) = bit_parsers::take(5usize)(input)?;
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
fn operating_parameters_info(
    input: BitInput,
    buffer_delay_length: usize,
) -> IResult<BitInput, (), Error<BitInput>> {
    let (input, _decoder_buffer_delay): (_, u64) = bit_parsers::take(buffer_delay_length)(input)?;
    let (input, _encoder_buffer_delay): (_, u64) = bit_parsers::take(buffer_delay_length)(input)?;
    let (input, _low_delay_mode_flag) = take_bool_bit(input)?;
    Ok((input, ()))
}

#[allow(clippy::too_many_lines)]
/// Parses AV1 `color_config` and normalizes chroma layout metadata.
///
/// ASSUMPTION: enum-coded color fields are spec-valid; invalid values currently
/// cause a panic via `unwrap()` because this parser treats malformed bitstreams
/// as unrecoverable.
fn color_config(
    input: BitInput,
    seq_profile: u8,
) -> IResult<BitInput, ColorConfig, Error<BitInput>> {
    let bit_depth: u8;
    let (input, high_bitdepth) = take_bool_bit(input)?;
    let input = if seq_profile == 2 && high_bitdepth {
        let (input, twelve_bit) = take_bool_bit(input)?;
        bit_depth = if twelve_bit { 12 } else { 10 };
        input
    } else {
        bit_depth = if high_bitdepth { 10 } else { 8 };
        input
    };
    let (input, monochrome) = if seq_profile == 1 {
        (input, false)
    } else {
        take_bool_bit(input)?
    };
    let num_planes = if monochrome { 1 } else { 3 };
    let (input, color_description_present_flag) = take_bool_bit(input)?;
    let (input, (color_primaries, transfer_characteristics, matrix_coefficients)) =
        if color_description_present_flag {
            let (input, color_primaries): (_, u8) = bit_parsers::take(8usize)(input)?;
            let (input, transfer_characteristics): (_, u8) = bit_parsers::take(8usize)(input)?;
            let (input, matrix_coefficients): (_, u8) = bit_parsers::take(8usize)(input)?;
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
        let (input, color_range): (_, u8) = bit_parsers::take(1usize)(input)?;
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
        let (input, color_range): (_, u8) = bit_parsers::take(1usize)(input)?;
        let (input, ss_x, ss_y) = if seq_profile == 0 {
            (input, 1, 1)
        } else if seq_profile == 1 {
            (input, 0, 0)
        } else if bit_depth == 12 {
            let (input, ss_x): (_, u8) = bit_parsers::take(1usize)(input)?;
            let (input, ss_y): (_, u8) = if ss_x > 0 {
                bit_parsers::take(1usize)(input)?
            } else {
                (input, 0)
            };
            (input, ss_x, ss_y)
        } else {
            (input, 1, 0)
        };
        let input = if ss_x > 0 && ss_y > 0 {
            let (input, _chroma_sample_position): (_, u8) = bit_parsers::take(2usize)(input)?;
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
    let (input, separate_uv_delta_q) = take_bool_bit(input)?;
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
        ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristics, color_config,
        decoder_model_info, operating_parameters_info, timing_info,
    };

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
        let (remaining, parsed) =
            timing_info((&data, 0)).expect("expected timing_info without uvlc payload");

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
        let (remaining, parsed) =
            timing_info((&data, 0)).expect("expected timing_info with uvlc payload");

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
        assert!(timing_info((&data, 3)).is_err());
    }

    #[test]
    fn decoder_model_info_parses_all_fields() {
        let mut bits = BitBuilder::default();
        bits.push_bits(17, 5);
        bits.push_bits(0x1234_5678, 32);
        bits.push_bits(7, 5);
        bits.push_bits(31, 5);

        let (data, consumed_bits) = with_trailer(bits);
        let (remaining, parsed) =
            decoder_model_info((&data, 0)).expect("expected decoder_model_info to parse");

        assert_eq!(parsed.buffer_delay_length_minus_1, 17);
        assert_eq!(parsed.buffer_removal_time_length_minus_1, 7);
        assert_eq!(parsed.frame_presentation_time_length_minus_1, 31);
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn decoder_model_info_returns_error_when_input_is_too_short() {
        let data = [0u8; 5];
        assert!(decoder_model_info((&data, 0)).is_err());
    }

    #[test]
    fn operating_parameters_info_parses_with_non_zero_buffer_delay_length() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0b1_0101, 5);
        bits.push_bits(0b0_0110, 5);
        bits.push_bool(true);

        let (data, consumed_bits) = with_trailer(bits);
        let (remaining, ()) = operating_parameters_info((&data, 0), 5)
            .expect("expected operating_parameters_info to parse");

        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn operating_parameters_info_supports_zero_buffer_delay_length() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false);

        let (data, consumed_bits) = with_trailer(bits);
        let (remaining, ()) = operating_parameters_info((&data, 0), 0)
            .expect("expected operating_parameters_info to parse with zero-width delays");

        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn operating_parameters_info_returns_error_when_low_delay_flag_is_missing() {
        let data = [0u8; 1];
        assert!(operating_parameters_info((&data, 2), 3).is_err());
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
        let (remaining, parsed) =
            color_config((&data, 0), 2).expect("expected monochrome color_config");

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
        let (remaining, parsed) =
            color_config((&data, 0), 2).expect("expected srgb identity color_config");

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
        let (remaining, parsed) =
            color_config((&data, 0), 0).expect("expected profile 0 color_config");

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
        let (remaining, parsed) =
            color_config((&data, 0), 1).expect("expected profile 1 color_config");

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
        let (remaining, parsed) =
            color_config((&data, 0), 2).expect("expected profile 2 12-bit ss_x=0 color_config");

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
        let (remaining, parsed) = color_config((&data, 0), 2)
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
        let (remaining, parsed) =
            color_config((&data, 0), 2).expect("expected profile 2 10-bit color_config");

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
        assert!(color_config((&data, 0), 0).is_err());
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
        _ = color_config((&data, 0), 0);
    }
}
