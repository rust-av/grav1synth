use arrayvec::ArrayVec;
use nom::{
    bits::{bits, complete as bit_parsers},
    IResult,
};
use num_enum::TryFromPrimitive;

use super::{
    grain::{film_grain_params, FilmGrainHeader},
    obu::ObuHeader,
    sequence::{SequenceHeader, SELECT_INTEGER_MV, SELECT_SCREEN_CONTENT_TOOLS},
    util::{take_bool_bit, BitInput},
};

const REFS_PER_FRAME: usize = 7;
const NUM_REF_FRAMES: usize = 8;
const REFRESH_ALL_FRAMES: u8 = 0b1111_1111;
const PRIMARY_REF_NONE: u8 = 7;

#[derive(Debug, Clone)]
pub struct FrameHeader {
    show_existing_frame: bool,
    film_grain_params: FilmGrainHeader,
}

/// This will return `None` for a show-existing frame. We don't need to apply
/// film grain params to those packets, because they are inherited from the ref
/// frame.
pub fn parse_frame_header<'a>(
    input: &'a [u8],
    seen_frame_header: &'a mut bool,
    sequence_headers: &SequenceHeader,
    obu_headers: &ObuHeader,
) -> IResult<&'a [u8], Option<FrameHeader>> {
    if *seen_frame_header {
        return Ok((input, None));
    }

    *seen_frame_header = true;
    bits(|input| {
        let (input, header) = uncompressed_header(input, sequence_headers, obu_headers)?;
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
    obu_headers: &ObuHeader,
) -> IResult<BitInput<'a>, FrameHeader> {
    let id_len = if sequence_headers.frame_id_numbers_present {
        Some(
            sequence_headers.additional_frame_id_len_minus_1
                + sequence_headers.delta_frame_id_len_minus_2
                + 3,
        )
    } else {
        None
    };

    let (input, frame_type, show_frame, show_existing_frame, error_resilient_mode) =
        if sequence_headers.reduced_still_picture_header {
            (input, FrameType::Inter, true, false, false)
        } else {
            let (input, show_existing_frame) = take_bool_bit(input)?;
            if show_existing_frame {
                let (input, _frame_to_show_map_idx): (_, u8) = bit_parsers::take(3usize)(input)?;
                let input = if let Some(id_len) = id_len {
                    let (input, display_frame_id) = bit_parsers::take(id_len)(input)?;
                    input
                } else {
                    input
                };
                return Ok((input, FrameHeader {
                    show_existing_frame,
                    film_grain_params: FilmGrainHeader::Disable,
                }));
            };
            let (input, frame_type): (_, u8) = bit_parsers::take(2usize)(input)?;
            let frame_type = FrameType::try_from(frame_type).unwrap();
            let (input, show_frame) = take_bool_bit(input)?;
            let input = if show_frame
                && sequence_headers.decoder_model_info.is_some()
                && !sequence_headers
                    .timing_info
                    .map(|ti| ti.equal_picture_interval)
                    .unwrap_or(false)
            {
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
            let (input, error_resilient_mode) = if frame_type == FrameType::Switch
                || (frame_type == FrameType::Key && show_frame)
            {
                (input, true)
            } else {
                take_bool_bit(input)?
            };
            (
                input,
                frame_type,
                show_frame,
                show_existing_frame,
                error_resilient_mode,
            )
        };

    let (input, disable_cdf_update) = take_bool_bit(input)?;
    let (input, allow_screen_content_tools) =
        if sequence_headers.force_screen_content_tools == SELECT_SCREEN_CONTENT_TOOLS {
            take_bool_bit(input)?
        } else {
            (input, sequence_headers.force_screen_content_tools == 1)
        };
    let input =
        if allow_screen_content_tools && sequence_headers.force_integer_mv == SELECT_INTEGER_MV {
            take_bool_bit(input)?.0
        } else {
            input
        };
    let input = if sequence_headers.frame_id_numbers_present {
        let (input, _current_frame_id): (_, usize) = bit_parsers::take(id_len.unwrap())(input)?;
        input
    } else {
        input
    };
    let (input, frame_size_override_flag) = if frame_type == FrameType::Switch {
        (input, true)
    } else if sequence_headers.reduced_still_picture_header {
        (input, false)
    } else {
        take_bool_bit(input)?
    };
    let (input, _order_hint): (_, u64) =
        bit_parsers::take(sequence_headers.order_hint_bits)(input)?;
    let (input, primary_ref_frame) = if frame_type.is_intra() || error_resilient_mode {
        (input, PRIMARY_REF_NONE)
    } else {
        bit_parsers::take(3usize)(input)?
    };

    let mut input = input;
    if let Some(decoder_model_info) = sequence_headers.decoder_model_info {
        let (input, buffer_removal_time_present_flag) = take_bool_bit(input)?;
        if buffer_removal_time_present_flag {
            for op_num in 0..=sequence_headers.operating_points_cnt_minus_1 {
                if sequence_headers.decoder_model_present_for_op[op_num] {
                    let op_pt_idc = sequence_headers.operating_point_idc[op_num];
                    let temporal_id = obu_headers
                        .extension
                        .map(|ext| ext.temporal_id)
                        .unwrap_or(0);
                    let spatial_id = obu_headers.extension.map(|ext| ext.spatial_id).unwrap_or(0);
                    let in_temporal_layer = (op_pt_idc >> temporal_id) & 1 > 0;
                    let in_spatial_layer = (op_pt_idc >> (spatial_id + 8)) & 1 > 0;
                    if op_pt_idc == 0 || (in_temporal_layer && in_spatial_layer) {
                        let n = decoder_model_info.buffer_removal_time_length_minus_1 + 1;
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
    if (!frame_type.is_intra() || refresh_frame_flags != REFRESH_ALL_FRAMES)
        && error_resilient_mode
        && sequence_headers.enable_order_hint()
    {
        for _ in 0..NUM_REF_FRAMES {
            let (inner_input, ref_order_hint): (_, u64) =
                bit_parsers::take(sequence_headers.order_hint_bits)(input)?;
            input = inner_input;
        }
    }
    let (input, use_ref_frame_mvs, ref_frame_idx) = if frame_type.is_intra() {
        let (input, frame_size) = frame_size(input)?;
        let (input, render_size) = render_size(input)?;
        (
            if allow_screen_content_tools && render_size.upscaled_width == frame_size.frame_width {
                let (input, allow_intrabc) = take_bool_bit(input)?;
                input
            } else {
                input
            },
            false,
            ArrayVec::new(),
        )
    } else {
        let (mut input, frame_refs_short_signaling) = if !sequence_headers.enable_order_hint() {
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
        let mut ref_frame_idx: ArrayVec<u8, REFS_PER_FRAME> = ArrayVec::new();
        for _ in 0..REFS_PER_FRAME {
            if !frame_refs_short_signaling {
                let (inner_input, this_ref_frame_idx) = bit_parsers::take(3usize)(input)?;
                input = inner_input;
                ref_frame_idx.push(this_ref_frame_idx);
                if sequence_headers.frame_id_numbers_present {
                    let n = sequence_headers.delta_frame_id_len_minus_2 + 2;
                    let (inner_input, _delta_frame_id_minus_1): (_, u64) =
                        bit_parsers::take(n)(input)?;
                    input = inner_input;
                }
            } else {
                ref_frame_idx.push(0);
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
        let (input, allow_high_precision_mv) = if sequence_headers.force_integer_mv == 1 {
            (input, false)
        } else {
            take_bool_bit(input)?
        };
        let (input, _) = read_interpolation_filter(input)?;
        let (input, is_motion_mode_switchable) = take_bool_bit(input)?;
        let (input, use_ref_frame_mvs) =
            if error_resilient_mode || !sequence_headers.enable_ref_frame_mvs {
                (input, false)
            } else {
                take_bool_bit(input)?
            };
        (input, use_ref_frame_mvs, ref_frame_idx)
    };

    let (input, disable_frame_end_update_cdf) =
        if sequence_headers.reduced_still_picture_header || disable_cdf_update {
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
    let (input, allow_warped_motion) = if frame_type.is_intra()
        || error_resilient_mode
        || !sequence_headers.enable_warped_motion
    {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum FrameType {
    Key,
    Inter,
    IntraOnly,
    Switch,
}

impl FrameType {
    pub fn is_intra(self) -> bool {
        self == FrameType::Key || self == FrameType::IntraOnly
    }
}

fn decode_frame_wrapup(input: BitInput) -> IResult<BitInput, ()> {
    // I don't believe this actually parses anything
    // or does anything relevant to us...
    Ok((input, ()))
}

fn temporal_point_info(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn frame_size(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn render_size(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn set_frame_refs(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn frame_size_with_refs(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn read_interpolation_filter(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn init_non_coeff_cdfs(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn setup_past_independence(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn load_cdfs(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn load_previous(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn motion_field_estimation(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn tile_info(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn quantization_params(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn segmentation_params(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn delta_q_params(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn delta_lf_params(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn init_coeff_cdfs(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn load_previous_segment_ids(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn loop_filter_params(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn cdef_params(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn lr_params(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn read_tx_mode(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn frame_reference_mode(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn skip_mode_params(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}

fn global_motion_params(input: BitInput) -> IResult<BitInput, ()> {
    todo!()
}
