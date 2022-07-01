use nom::{
    bits::{bits, complete as bit_parsers},
    IResult,
};

use super::{
    grain::{film_grain_params, FilmGrainHeader},
    sequence::SequenceHeader,
    util::{take_bool_bit, BitInput},
};

#[derive(Debug, Clone)]
pub struct FrameHeader {
    show_existing_frame: bool,
    film_grain_params: Option<FilmGrainHeader>,
}

/// This will return `None` for a show-existing frame. We don't need to apply
/// film grain params to those packets, because they are inherited from the ref
/// frame.
pub fn parse_frame_header<'a>(
    input: &'a [u8],
    seen_frame_header: &'a mut bool,
    sequence_headers: &SequenceHeader,
) -> IResult<&'a [u8], Option<FrameHeader>> {
    if *seen_frame_header {
        return Ok((input, None));
    }

    *seen_frame_header = true;
    bits(|input| {
        let (input, header) = uncompressed_header(input, sequence_headers)?;
        if header.show_existing_frame {
            let (input, _) = decode_frame_wrapup(input)?;
            *seen_frame_header = false;
            Ok((input, Some(header)))
        } else {
            *seen_frame_header = true;
            Ok((input, Some(header)))
        }
    })(input)
}

#[allow(clippy::fn_params_excessive_bools)]
fn uncompressed_header<'a>(
    input: BitInput<'a>,
    sequence_headers: &'a SequenceHeader,
) -> IResult<BitInput<'a>, FrameHeader> {
    let id_len = if frame_id_numbers_present_flag {
        Some(additional_frame_id_length_minus_1 + delta_frame_id_length_minus_2 + 3)
    } else {
        None
    };

    let (input, show_existing_frame) = if reduced_still_picture_header {
        (input, false)
    } else {
        let (input, show_existing_frame) = take_bool_bit(input)?;
        let input = if show_existing_frame {
            let (input, _frame_to_show_map_idx): (_, u8) = bit_parsers::take(3usize)(input)?;
            if let Some(id_len) = id_len {
                let (input, display_frame_id) = bit_parsers::take(id_len)(input)?;
                (input, Some(display_frame_id))
            } else {
                (input, None)
            }
            .0
        } else {
            input
        };
        let (input, frame_type): (_, u8) = bit_parsers::take(2usize)(input)?;
        let (input, show_frame) = take_bool_bit(input)?;
        let input = if show_frame && decoder_model_info_present_flag && !equal_picture_interval {
            temporal_point_info(input)?.0
        } else {
            input
        };
        let input = if show_frame {
            input
        } else {
            let (input, _showable_frame) = take_bool_bit(input)?;
            input
        };
        let input =
            if frame_type == FrameType::Switch || (frame_type == FrameType::Key && show_frame) {
                input
            } else {
                let (input, _error_resilient_mode) = take_bool_bit(input)?;
                input
            };
    };

    let (input, disable_cdf_update) = take_bool_bit(input)?;
    let (input, allow_screen_content_tools) =
        if seq_force_screen_content_tools == SELECT_SCREEN_CONTENT_TOOLS {
            take_bool_bit(input)?
        } else {
            (input, seq_force_screen_content_tools)
        };
    let input = if allow_screen_content_tools && seq_force_integer_mv == SELECT_INTEGER_MV {
        take_bool_bit(input)?.0
    } else {
        input
    };
    let input = if frame_id_numbers_present_flag {
        let (input, _current_frame_id): (_, usize) = bit_parsers::take(id_len)(input)?;
        input
    } else {
        input
    };
    let (input, frame_size_override_flag) =
        if frame_type == FrameType::Switch || reduced_still_picture_header {
            input
        } else {
            take_bool_bit(input)?
        };
    let (input, _order_hint): (_, u64) = bit_parsers::take(order_hint_bits)(input)?;
    let input = if frame_type.is_intra() || error_resilient_mode {
        input
    } else {
        let (input, primary_ref_frame): (_, u8) = bit_parsers::take(3usize)(input)?;
        input
    };

    let mut input = input;
    if decoder_model_info_present_flag {
        let (input, buffer_removal_time_present_flag) = take_bool_bit(input)?;
        if buffer_removal_time_present_flag {
            for op_num in 0..=operating_points_cnt_minus_1 {
                if decoder_model_present_for_this_op[op_num] {
                    let op_pt_idc = operating_point_idc[op_num];
                    let in_temporal_layer = (op_pt_idc >> temporal_id) & 1 > 0;
                    let in_spatial_layer = (op_pt_idc >> (spatial_id + 8)) & 1 > 0;
                    if op_pt_idc == 0 || (in_temporal_layer && in_spatial_layer) {
                        let n = buffer_removal_time_length_minus_1 + 1;
                        let (inner_input, _buffer_removal_time): (_, u64) =
                            bit_parsers::take(n)(input)?;
                        input = inner_input;
                    }
                }
            }
        }
    }

    let (input, refresh_frame_flags): (_, u8) =
        if frame_type == FrameType::Switch || (frame_type == FrameType::Key && show_frame) {
            (input, REFRESH_ALL_FRAMES)
        } else {
            bit_parsers::take(8)(input)?
        };

    let mut input = input;
    if !frame_type.is_intra() || refresh_frame_flags != REFRESH_ALL_FRAMES {
        if error_resilient_mode && enable_order_hint {
            for _ in 0..NUM_REF_FRAMES {
                let (inner_input, ref_order_hint): (_, u64) =
                    bit_parsers::take(order_hint_bits)(input)?;
                input = inner_input;
            }
        }
    }
    let input = if frame_type.is_intra() {
        let (input, frame_size) = frame_size(input)?;
        let (input, render_size) = render_size(input)?;
        if allow_screen_content_tools && render_size.upscaled_width == frame_size.frame_width {
            let (input, allow_intrabc) = take_bool_bit(input)?;
            input
        } else {
            input
        }
    } else {
        let (mut input, frame_refs_short_signaling) = if !enable_order_hint {
            (input, false)
        } else {
            let (input, frame_refs_short_signaling) = take_bool_bit(input)?;
            if frame_refs_short_signaling {
                let (input, last_frame_idx) = bit_parsers::take(3usize)(input)?;
                let (input, gold_frame_idx) = bit_parsers::take(3usize)(input)?;
                let (input, _) = set_frame_refs(input, last_frame_idx, gold_frame_idx)?;
                (input, frame_refs_short_signaling)
            } else {
                (input, frame_refs_short_signaling)
            }
        };
        for _ in 0..REFS_PER_FRAME {
            if !frame_refs_short_signaling {
                let (inner_input, ref_frame_idx) = bit_parsers::take(3usize)(input)?;
                input = inner_input;
                if frame_id_numbers_present_flag {
                    let n = delta_frame_id_length_minus_2 + 2;
                    let (inner_input, _delta_frame_id_minus_1): (_, u64) =
                        bit_parsers::take(n)(input)?;
                    input = inner_input;
                }
            }
        }
        let input = if frame_size_override_flag && !error_resilient_mode {
            let (input, _) = frame_size_with_refs(input)?;
            input
        } else {
            let (input, frame_size) = frame_size(input)?;
            let (input, render_size) = render_size(input)?;
            input
        };
        let (input, allow_high_precision_mv) = if force_integer_mv {
            (input, false)
        } else {
            take_bool_bit(input)?
        };
        let (input, _) = read_interpolation_filter(input)?;
        let (input, is_motion_mode_switchable) = take_bool_bit(input)?;
        let (input, use_ref_frame_mvs) = if error_resilient_mode || !enable_ref_frame_mvs {
            (input, false)
        } else {
            take_bool_bit(input)?
        };
        input
    };

    let (input, disable_frame_end_update_cdf) =
        if reduced_still_picture_header || disable_cdf_update {
            (input, true)
        } else {
            take_bool_bit(input)?
        };
    let input = if primary_ref_frame == PRIMARY_REF_NONE {
        let (input, _) = init_non_coeff_cdfs(input)?;
        let (input, _) = setup_past_independence(input)?;
        input
    } else {
        let (input, _) = load_cdfs(input, ref_frame_idx[primary_ref_frame])?;
        let (input, _) = load_previous(input)?;
    };
    let input = if use_ref_frame_mvs {
        motion_field_estimation(input)?.0
    } else {
        input
    };
    let (input, _) = tile_info(input)?;
    let (input, _) = quantization_params(input)?;
    let (input, _) = segmentation_params(input)?;
    let (input, _) = delta_q_params(input)?;
    let (input, _) = delta_lf_params(input)?;
    let input = if primary_ref_frame == PRIMARY_REF_NONE {
        init_coeff_cdfs(input)?.0
    } else {
        load_previous_segment_ids(input)?.0
    };

    let (input, _) = loop_filter_params(input)?;
    let (input, _) = cdef_params(input)?;
    let (input, _) = lr_params(input)?;
    let (input, _) = read_tx_mode(input)?;
    let (input, _) = frame_reference_mode(input)?;
    let (input, _) = skip_mode_params(input)?;
    let (input, allow_warped_motion) =
        if frame_type.is_intra() || error_resilient_mode || !enable_warped_motion {
            (input, false)
        } else {
            take_bool_bit(input)?
        };
    let (input, reduced_tx_set) = take_bool_bit(input)?;
    let (input, _) = global_motion_params(input)?;
    let (input, film_grain_params) = film_grain_params(input)?;

    Ok((input, FrameHeader {
        show_existing_frame,
        film_grain_params,
    }))
}

fn decode_frame_wrapup(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}
