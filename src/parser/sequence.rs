use nom::{bits, bits::complete as bit_parsers, IResult};
use num_enum::TryFromPrimitive;

use super::util::{take_bool_bit, uvlc, BitInput};

const SELECT_SCREEN_CONTENT_TOOLS: u8 = 2;

#[derive(Debug, Clone, Copy)]
pub struct SequenceHeader {
    pub film_grain_params_present: bool,
}

#[allow(clippy::too_many_lines)]
pub fn parse_sequence_header(input: &[u8]) -> IResult<&[u8], SequenceHeader> {
    bits(|input| {
        let (input, seq_profile): (_, u8) = bit_parsers::take(3usize)(input)?;
        let (input, _still_picture) = take_bool_bit(input)?;
        let (input, reduced_still_picture_header) = take_bool_bit(input)?;
        let input = if reduced_still_picture_header {
            let (input, _seq_level_idx): (_, u8) = bit_parsers::take(5usize)(input)?;
            input
        } else {
            let mut buffer_delay_length = 0;
            let (input, timing_info_present_flag) = take_bool_bit(input)?;
            let (input, decoder_model_info_present_flag) = if timing_info_present_flag {
                let input = timing_info(input)?.0;
                let (input, flag) = take_bool_bit(input)?;
                let input = if flag {
                    let (input, decoder_model) = decoder_model_info(input)?;
                    buffer_delay_length = decoder_model.buffer_delay_length_minus_1 as usize + 1;
                    input
                } else {
                    input
                };
                (input, flag)
            } else {
                (input, false)
            };
            let (input, initial_display_delay_present_flag) = take_bool_bit(input)?;
            let (mut input, operating_points_cnt_minus_1): (_, u8) =
                bit_parsers::take(5usize)(input)?;
            for _ in 0..=operating_points_cnt_minus_1 {
                let inner_input = input;
                let (inner_input, _operating_point_idc): (_, u16) =
                    bit_parsers::take(12usize)(inner_input)?;
                let (inner_input, seq_level_idx): (_, u8) = bit_parsers::take(5usize)(inner_input)?;
                let (inner_input, _seq_tier) = if seq_level_idx > 7 {
                    take_bool_bit(inner_input)?
                } else {
                    (inner_input, false)
                };
                let (inner_input, _decoder_model_present_for_op) =
                    if decoder_model_info_present_flag {
                        let (inner_input, flag) = take_bool_bit(inner_input)?;
                        if flag {
                            (
                                operating_parameters_info(inner_input, buffer_delay_length)?.0,
                                flag,
                            )
                        } else {
                            (inner_input, flag)
                        }
                    } else {
                        (inner_input, false)
                    };
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
            input
        };

        let (input, frame_width_bits_minus_1): (_, u8) = bit_parsers::take(4usize)(input)?;
        let (input, frame_height_bits_minus_1): (_, u8) = bit_parsers::take(4usize)(input)?;
        let (input, _max_frame_width_minus_1): (_, u64) =
            bit_parsers::take(frame_width_bits_minus_1 + 1)(input)?;
        let (input, _max_frame_height_minus_1): (_, u64) =
            bit_parsers::take(frame_height_bits_minus_1 + 1)(input)?;
        let (input, frame_id_numbers_present) = if reduced_still_picture_header {
            (input, false)
        } else {
            take_bool_bit(input)?
        };
        let input = if frame_id_numbers_present {
            let (input, _delta_frame_id_len_minus_2): (_, u8) = bit_parsers::take(4usize)(input)?;
            let (input, _additional_frame_id_len_minus_1): (_, u8) =
                bit_parsers::take(3usize)(input)?;
            input
        } else {
            input
        };
        let (input, _use_128x128_superblock) = take_bool_bit(input)?;
        let (input, _enable_filter_intra) = take_bool_bit(input)?;
        let (input, _enable_intra_edge_filter) = take_bool_bit(input)?;
        let input = if reduced_still_picture_header {
            input
        } else {
            let (input, _enable_interintra_compound) = take_bool_bit(input)?;
            let (input, _enable_masked_compound) = take_bool_bit(input)?;
            let (input, _enable_warped_motion) = take_bool_bit(input)?;
            let (input, _enable_dual_filter) = take_bool_bit(input)?;
            let (input, enable_order_hint) = take_bool_bit(input)?;
            let input = if enable_order_hint {
                let (input, _enable_jnt_comp) = take_bool_bit(input)?;
                let (input, _enable_ref_frame_mvs) = take_bool_bit(input)?;
                input
            } else {
                input
            };
            let (input, seq_choose_screen_content_tools) = take_bool_bit(input)?;
            let (input, seq_force_screen_content_tools): (_, u8) =
                if seq_choose_screen_content_tools {
                    (input, SELECT_SCREEN_CONTENT_TOOLS)
                } else {
                    bit_parsers::take(1usize)(input)?
                };

            let input = if seq_force_screen_content_tools > 0 {
                let (input, seq_choose_integer_mv) = take_bool_bit(input)?;
                if seq_choose_integer_mv {
                    input
                } else {
                    let (input, _seq_force_screen_content_tools): (_, u8) =
                        bit_parsers::take(1usize)(input)?;
                    input
                }
            } else {
                input
            };
            let (input, _order_hint_bits_minus_1) = if enable_order_hint {
                bit_parsers::take(3usize)(input)?
            } else {
                (input, 0u8)
            };

            input
        };

        let (input, _enable_superres) = take_bool_bit(input)?;
        let (input, _enable_cdef) = take_bool_bit(input)?;
        let (input, _enable_restoration) = take_bool_bit(input)?;
        let input = color_config(input, seq_profile)?.0;
        let (input, film_grain_params_present) = take_bool_bit(input)?;

        Ok((input, SequenceHeader {
            film_grain_params_present,
        }))
    })(input)
}

fn timing_info(input: BitInput) -> IResult<BitInput, ()> {
    let (input, _num_units_in_display_tick): (_, u32) = bit_parsers::take(32usize)(input)?;
    let (input, _time_scale): (_, u32) = bit_parsers::take(32usize)(input)?;
    let (input, equal_picture_interval) = take_bool_bit(input)?;
    if equal_picture_interval {
        let (input, _num_ticks_per_picture_minus_1) = uvlc(input)?;
        Ok((input, ()))
    } else {
        Ok((input, ()))
    }
}

fn decoder_model_info(input: BitInput) -> IResult<BitInput, DecoderModelInfo> {
    let (input, buffer_delay_length_minus_1): (_, u8) = bit_parsers::take(5usize)(input)?;
    let (input, _num_units_in_decoding_tick): (_, u32) = bit_parsers::take(32usize)(input)?;
    let (input, _buffer_removal_time_length_minus_1): (_, u8) = bit_parsers::take(5usize)(input)?;
    let (input, _frame_presentation_time_length_minus_1): (_, u8) =
        bit_parsers::take(5usize)(input)?;
    Ok((input, DecoderModelInfo {
        buffer_delay_length_minus_1,
    }))
}

#[derive(Debug, Clone, Copy)]
struct DecoderModelInfo {
    buffer_delay_length_minus_1: u8,
}

fn operating_parameters_info(input: BitInput, buffer_delay_length: usize) -> IResult<BitInput, ()> {
    let (input, _decoder_buffer_delay): (_, u64) = bit_parsers::take(buffer_delay_length)(input)?;
    let (input, _encoder_buffer_delay): (_, u64) = bit_parsers::take(buffer_delay_length)(input)?;
    let (input, _low_delay_mode_flag) = take_bool_bit(input)?;
    Ok((input, ()))
}

fn color_config(input: BitInput, seq_profile: u8) -> IResult<BitInput, ColorConfig> {
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
    let (input, color_range) = if monochrome {
        let (input, color_range): (_, u8) = bit_parsers::take(1usize)(input)?;
        return Ok((input, ColorConfig {
            color_primaries,
            transfer_characteristics,
            matrix_coefficients,
            color_range: ColorRange::try_from(color_range).unwrap(),
        }));
    } else if color_primaries == ColorPrimaries::Bt709
        && transfer_characteristics == TransferCharacteristics::Srgb
        && matrix_coefficients == MatrixCoefficients::Identity
    {
        (input, ColorRange::Full)
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
        (input, ColorRange::try_from(color_range).unwrap())
    };
    let (input, _separate_uv_delta_q) = take_bool_bit(input)?;
    Ok((input, ColorConfig {
        color_primaries,
        transfer_characteristics,
        matrix_coefficients,
        color_range,
    }))
}

#[derive(Debug, Clone, Copy)]
pub struct ColorConfig {
    pub color_primaries: ColorPrimaries,
    pub transfer_characteristics: TransferCharacteristics,
    pub matrix_coefficients: MatrixCoefficients,
    pub color_range: ColorRange,
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
