use nom::{bits, bits::complete as bit_parsers, IResult};

use super::util::take_bool_bit;

#[derive(Debug, Clone, Copy)]
pub struct SequenceHeader {
    film_grain_params_present: bool,
}

pub fn parse_sequence_header(input: &[u8], size: usize) -> IResult<&[u8], SequenceHeader> {
    let (input, header) = bits(|input| {
        let (input, seq_header): (_, u8) = bit_parsers::take(3usize)(input)?;
        let (input, still_picture) = take_bool_bit(input)?;
        let (input, reduced_still_picture_header) = take_bool_bit(input)?;
        let input = if reduced_still_picture_header {
            let (input, _seq_level_idx) = bit_parsers::take(5usize)(input)?;
            input
        } else {
            let (input, timing_info_present_flag) = take_bool_bit(input)?;
            let (input, decoder_model_info_present_flag) = if timing_info_present_flag {
                let input = timing_info(input)?.0;
                let (input, flag) = take_bool_bit(input)?;
                let input = if flag {
                    decoder_model_info(input)?.0
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
            for i in 0..=operating_points_cnt_minus_1 {
                let inner_input = input;
                let (inner_input, operating_point_idc) = bit_parsers::take(12usize)(inner_input)?;
                let (inner_input, seq_level_idx) = bit_parsers::take(5usize)(inner_input)?;
                let (inner_input, _seq_tier) = if seq_level_idx > 7 {
                    take_bool_bit(inner_input)?
                } else {
                    (inner_input, false)
                };
                let (inner_input, _decoder_model_present_for_op) =
                    if decoder_model_info_present_flag {
                        let (inner_input, flag) = take_bool_bit(inner_input)?;
                        if flag {
                            (operating_parameters_info(inner_input)?.0, flag)
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
                input = inner_input
            }
        };

        let (input, frame_width_bits_minus_1): (_, u8) = bit_parsers::take(4usize)(input)?;
        let (input, frame_height_bits_minus_1): (_, u8) = bit_parsers::take(4usize)(input)?;
        let (input, max_frame_width_minus_1): (_, u64) =
            bit_parsers::take(frame_width_bits_minus_1 + 1)(input)?;
        let (input, max_frame_height_minus_1): (_, u64) =
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
                    (input, SELECT_INTEGER_MV)
                } else {
                    bit_parsers::take(1usize)(input)?
                }
            };
            let (input, _order_hint_bits_minus_1) = if enable_order_hint {
                bit_parsers::take(3usize)(input)?
            } else {
                (input, 0)
            };

            input
        };

        let (input, _enable_superres) = take_bool_bit(input)?;
        let (input, _enable_cdef) = take_bool_bit(input)?;
        let (input, _enable_restoration) = take_bool_bit(input)?;
        let input = color_config(input)?.0;
        let (input, film_grain_params_present) = take_bool_bit(input)?;

        Ok((input, SequenceHeader {
            film_grain_params_present,
        }))
    })(input)?;

    todo!()
}
