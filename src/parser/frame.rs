use std::cmp::{max, min};

use arrayvec::ArrayVec;
use nom::{
    bits::{bits, complete as bit_parsers},
    IResult,
};
use num_enum::TryFromPrimitive;
use num_traits::{clamp, PrimInt};

use super::{
    grain::{film_grain_params, FilmGrainHeader},
    obu::ObuHeader,
    sequence::{SequenceHeader, SELECT_INTEGER_MV, SELECT_SCREEN_CONTENT_TOOLS},
    util::{ns, su, take_bool_bit, BitInput},
};

const REFS_PER_FRAME: usize = 7;
const TOTAL_REFS_PER_FRAME: usize = 8;
const NUM_REF_FRAMES: usize = 8;
const REFRESH_ALL_FRAMES: u8 = 0b1111_1111;
const PRIMARY_REF_NONE: u8 = 7;

const SUPERRES_DENOM_BITS: usize = 3;
const SUPERRES_DENOM_MIN: u32 = 9;
const SUPERRES_NUM: u32 = 8;

const MAX_TILE_WIDTH: u32 = 4096;
const MAX_TILE_COLS: u32 = 64;
const MAX_TILE_ROWS: u32 = 64;
const MAX_TILE_AREA: u32 = 4096 * 2304;

const MAX_SEGMENTS: usize = 8;
const SEG_LVL_MAX: usize = 8;
const SEG_LVL_ALT_Q: usize = 0;
const SEGMENTATION_FEATURE_BITS: [u8; SEG_LVL_MAX] = [8, 6, 6, 6, 6, 3, 0, 0];
const SEGMENTATION_FEATURE_SIGNED: [bool; SEG_LVL_MAX] =
    [true, true, true, true, true, false, false, false];
const SEGMENTATION_FEATURE_MAX: [u8; SEG_LVL_MAX] = [
    255,
    MAX_LOOP_FILTER,
    MAX_LOOP_FILTER,
    MAX_LOOP_FILTER,
    MAX_LOOP_FILTER,
    7,
    0,
    0,
];
type SegmentationData = [[Option<i16>; SEG_LVL_MAX]; MAX_SEGMENTS];

const MAX_LOOP_FILTER: u8 = 63;
const RESTORE_NONE: u8 = 0;
const RESTORE_SWITCHABLE: u8 = 1;
const RESTORE_WIENER: u8 = 2;
const RESTORE_SGRPROJ: u8 = 3;

#[derive(Debug, Clone)]
pub struct FrameHeader {
    show_existing_frame: bool,
    film_grain_params: FilmGrainHeader,
}

/// This will return `None` for a show-existing frame. We don't need to apply
/// film grain params to those packets, because they are inherited from the ref
/// frame.
///
/// I wish we didn't have to parse the whole frame header,
/// but the film grain params are of course the very last item,
/// and we don't know how many bits precede it, so we have to parse
/// THE WHOLE THING before we get the film grain params.
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

    let (input, frame_type, show_frame, showable_frame, show_existing_frame, error_resilient_mode) =
        if sequence_headers.reduced_still_picture_header {
            (input, FrameType::Inter, true, true, false, false)
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
                temporal_point_info(
                    input,
                    sequence_headers
                        .decoder_model_info
                        .unwrap()
                        .frame_presentation_time_length_minus_1 as usize
                        + 1,
                )?
                .0
            } else {
                input
            };
            let (input, showable_frame) = if show_frame {
                (input, true)
            } else {
                take_bool_bit(input)?
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
                showable_frame,
                show_existing_frame,
                error_resilient_mode,
            )
        };

    let mut big_ref_order_hint: ArrayVec<u64, NUM_REF_FRAMES> = ArrayVec::new();
    let mut big_ref_valid: ArrayVec<bool, NUM_REF_FRAMES> = ArrayVec::new();
    let mut big_order_hints: ArrayVec<u64, { RefType::Last as usize + REFS_PER_FRAME }> =
        ArrayVec::new();
    if frame_type == FrameType::Key && show_frame {
        for _ in 0..NUM_REF_FRAMES {
            big_ref_valid.push(false);
            big_ref_order_hint.push(0);
        }
        big_order_hints.push(0);
        for _ in 0..REFS_PER_FRAME {
            big_order_hints.push(0);
        }
    }

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
    let (input, order_hint): (_, u64) = bit_parsers::take(sequence_headers.order_hint_bits)(input)?;
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

    let mut allow_intrabc = false;
    let (input, refresh_frame_flags): (_, u8) =
        if frame_type == FrameType::Switch || (frame_type == FrameType::Key && show_frame) {
            (input, REFRESH_ALL_FRAMES)
        } else {
            bit_parsers::take(8)(input)?
        };

    let mut ref_order_hint: ArrayVec<u64, NUM_REF_FRAMES> = ArrayVec::new();
    let mut input = input;
    if (!frame_type.is_intra() || refresh_frame_flags != REFRESH_ALL_FRAMES)
        && error_resilient_mode
        && sequence_headers.enable_order_hint()
    {
        for i in 0..NUM_REF_FRAMES {
            let (inner_input, cur_ref_order_hint): (_, u64) =
                bit_parsers::take(sequence_headers.order_hint_bits)(input)?;
            ref_order_hint.push(cur_ref_order_hint);
            if ref_order_hint[i] != big_ref_order_hint[i] {
                big_ref_valid[i] = false;
            }
            input = inner_input;
        }
    }

    let max_frame_size = Dimensions {
        width: sequence_headers.max_frame_width_minus_1 + 1,
        height: sequence_headers.max_frame_height_minus_1 + 1,
    };
    let (input, use_ref_frame_mvs, ref_frame_idx, frame_size, upscaled_size) =
        if frame_type.is_intra() {
            let (input, frame_size) = frame_size(
                input,
                frame_size_override_flag,
                sequence_headers.enable_superres,
                sequence_headers.frame_width_bits_minus_1 + 1,
                sequence_headers.frame_height_bits_minus_1 + 1,
                max_frame_size,
            )?;
            let mut upscaled_size = frame_size;
            let (input, render_size) = render_size(input, frame_size, &mut upscaled_size)?;
            (
                if allow_screen_content_tools && upscaled_size.width == frame_size.width {
                    let (input, allow_intrabc_inner) = take_bool_bit(input)?;
                    allow_intrabc = allow_intrabc_inner;
                    input
                } else {
                    input
                },
                false,
                ArrayVec::new(),
                frame_size,
                upscaled_size,
            )
        } else {
            let (mut input, frame_refs_short_signaling) = if !sequence_headers.enable_order_hint() {
                (input, false)
            } else {
                let (input, frame_refs_short_signaling) = take_bool_bit(input)?;
                if frame_refs_short_signaling {
                    let (input, last_frame_idx) = bit_parsers::take(3usize)(input)?;
                    let (input, gold_frame_idx) = bit_parsers::take(3usize)(input)?;
                    let (input, _) = set_frame_refs(input)?;
                    (input, frame_refs_short_signaling)
                } else {
                    (input, frame_refs_short_signaling)
                }
            };
            let mut ref_frame_idx: ArrayVec<usize, REFS_PER_FRAME> = ArrayVec::new();
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
            let (input, frame_size, upscaled_size) =
                if frame_size_override_flag && !error_resilient_mode {
                    let frame_size = max_frame_size;
                    let mut upscaled_size = frame_size;
                    let (input, frame_size) = frame_size_with_refs(
                        input,
                        sequence_headers.enable_superres,
                        frame_size_override_flag,
                        sequence_headers.frame_width_bits_minus_1 + 1,
                        sequence_headers.frame_height_bits_minus_1 + 1,
                        max_frame_size,
                        &mut frame_size,
                        &mut upscaled_size,
                    )?;
                    (input, frame_size, upscaled_size)
                } else {
                    let (input, frame_size) = frame_size(
                        input,
                        frame_size_override_flag,
                        sequence_headers.enable_superres,
                        sequence_headers.frame_width_bits_minus_1 + 1,
                        sequence_headers.frame_height_bits_minus_1 + 1,
                        max_frame_size,
                    )?;
                    let mut upscaled_size = frame_size;
                    let (input, render_size) = render_size(input, frame_size, &mut upscaled_size)?;
                    (input, frame_size, upscaled_size)
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
            for i in 0..REFS_PER_FRAME {
                let ref_frame = RefType::Last as usize + i;
                let hint = big_ref_order_hint[ref_frame_idx[i]];
                big_order_hints[ref_frame] = hint;
                // don't think we care about ref frame sign bias
            }
            (
                input,
                use_ref_frame_mvs,
                ref_frame_idx,
                frame_size,
                upscaled_size,
            )
        };
    let (mi_cols, mi_rows) = compute_image_size(frame_size);

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
        let (input, _) = load_cdfs(input)?;
        let (input, _) = load_previous(input)?;
        input
    };
    let input = if use_ref_frame_mvs {
        motion_field_estimation(input)?.0
    } else {
        input
    };
    let (input, _) = tile_info(
        input,
        sequence_headers.use_128x128_superblock,
        mi_cols,
        mi_rows,
    )?;
    let (input, q_params) = quantization_params(
        input,
        sequence_headers.color_config.num_planes,
        sequence_headers.color_config.separate_uv_delta_q,
    )?;
    let (input, _) = segmentation_params(input, primary_ref_frame)?;
    let (input, delta_q_present) = delta_q_params(input, q_params.base_q_idx)?;
    let (input, _) = delta_lf_params(input, delta_q_present, allow_intrabc)?;
    let input = if primary_ref_frame == PRIMARY_REF_NONE {
        init_coeff_cdfs(input)?.0
    } else {
        load_previous_segment_ids(input)?.0
    };

    let mut coded_lossless = true;
    for segment_id in 0..MAX_SEGMENTS {
        let qindex = get_qindex(
            true,
            segment_id,
            q_params.base_q_idx,
            current_q_index,
            segmentation_enabled,
            segmentation_feature_data,
        );
        let lossless = qindex == 0
            && q_params.deltaq_y_dc == 0
            && q_params.deltaq_u_ac == 0
            && q_params.deltaq_u_dc == 0
            && q_params.deltaq_v_ac == 0
            && q_params.deltaq_v_dc == 0;
        if !lossless {
            coded_lossless = false;
            break;
        }
    }
    let all_losslesss = coded_lossless && frame_size.width == upscaled_size.width;
    let (input, _) = loop_filter_params(
        input,
        coded_lossless,
        allow_intrabc,
        sequence_headers.color_config.num_planes,
    )?;
    let (input, _) = cdef_params(
        input,
        coded_lossless,
        allow_intrabc,
        sequence_headers.enable_cdef,
        sequence_headers.color_config.num_planes,
    )?;
    let (input, _) = lr_params(
        input,
        all_losslesss,
        allow_intrabc,
        sequence_headers.enable_restoration,
        sequence_headers.use_128x128_superblock,
        sequence_headers.color_config.num_planes,
        sequence_headers.color_config.subsampling,
    )?;
    let (input, _) = read_tx_mode(input, coded_lossless)?;
    let (input, reference_select) = frame_reference_mode(input, frame_type.is_intra())?;
    let (input, _) = skip_mode_params(
        input,
        frame_type.is_intra(),
        reference_select,
        sequence_headers.order_hint_bits,
        order_hint,
        &big_ref_order_hint,
        &ref_frame_idx,
    )?;
    let (input, allow_warped_motion) = if frame_type.is_intra()
        || error_resilient_mode
        || !sequence_headers.enable_warped_motion
    {
        (input, false)
    } else {
        take_bool_bit(input)?
    };
    let (input, reduced_tx_set) = take_bool_bit(input)?;
    let (input, _) = global_motion_params(input, frame_type.is_intra())?;
    let (input, film_grain_params) = film_grain_params(
        input,
        sequence_headers.film_grain_params_present,
        show_frame,
        showable_frame,
        frame_type,
        sequence_headers.color_config.num_planes == 1,
        sequence_headers.color_config.subsampling,
    )?;

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

fn temporal_point_info(
    input: BitInput,
    frame_presentation_time_length: usize,
) -> IResult<BitInput, ()> {
    let (input, _frame_presentation_time): (_, u64) =
        bit_parsers::take(frame_presentation_time_length)(input)?;
    Ok((input, ()))
}

#[derive(Debug, Clone, Copy)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

fn frame_size(
    input: BitInput,
    frame_size_override: bool,
    enable_superres: bool,
    frame_width_bits: usize,
    frame_height_bits: usize,
    max_frame_size: Dimensions,
) -> IResult<BitInput, Dimensions> {
    let (input, width, height) = if frame_size_override {
        let (input, width_minus_1): (_, u32) = bit_parsers::take(frame_width_bits)(input)?;
        let (input, height_minus_1): (_, u32) = bit_parsers::take(frame_height_bits)(input)?;
        (input, width_minus_1 + 1, height_minus_1 + 1)
    } else {
        (input, max_frame_size.width, max_frame_size.height)
    };
    let mut frame_size = Dimensions { width, height };
    let mut upscaled_size = frame_size;
    let (input, _) = superres_params(input, enable_superres, &mut frame_size, &mut upscaled_size)?;
    Ok((input, frame_size))
}

fn render_size<'a>(
    input: BitInput<'a>,
    frame_size: Dimensions,
    upscaled_size: &'a mut Dimensions,
) -> IResult<BitInput<'a>, Dimensions> {
    let (input, render_and_frame_size_different) = take_bool_bit(input)?;
    let (input, width, height) = if render_and_frame_size_different {
        let (input, render_width_minus_1): (_, u32) = bit_parsers::take(16usize)(input)?;
        let (input, render_height_minus_1): (_, u32) = bit_parsers::take(16usize)(input)?;
        (input, render_width_minus_1 + 1, render_height_minus_1 + 1)
    } else {
        (input, upscaled_size.width, frame_size.height)
    };
    Ok((input, Dimensions { width, height }))
}

fn set_frame_refs(input: BitInput) -> IResult<BitInput, ()> {
    // Does nothing that we care about
    Ok((input, ()))
}

fn frame_size_with_refs<'a>(
    input: BitInput<'a>,
    enable_superres: bool,
    frame_size_override: bool,
    frame_width_bits: usize,
    frame_height_bits: usize,
    max_frame_size: Dimensions,
    ref_frame_size: &'a mut Dimensions,
    ref_upscaled_size: &'a mut Dimensions,
) -> IResult<BitInput<'a>, Dimensions> {
    let mut found_ref = false;
    for _ in 0..REFS_PER_FRAME {
        let (input, found_this_ref) = take_bool_bit(input)?;
        if found_this_ref {
            found_ref = true;
            // We don't actually care about the changes to frame size. But if we did, we'd
            // have to do things here.
            break;
        }
    }
    let (input, frame_size) = if !found_ref {
        let (input, frame_size) = frame_size(
            input,
            frame_size_override,
            enable_superres,
            frame_width_bits,
            frame_height_bits,
            max_frame_size,
        )?;
        let (input, _) = render_size(input, frame_size, ref_upscaled_size)?;
        (input, frame_size)
    } else {
        let (input, _) =
            superres_params(input, enable_superres, ref_frame_size, ref_upscaled_size)?;
        (input, *ref_frame_size)
    };
    Ok((input, frame_size))
}

fn superres_params<'a>(
    input: BitInput,
    enable_superres: bool,
    frame_size: &'a mut Dimensions,
    upscaled_size: &'a mut Dimensions,
) -> IResult<BitInput<'a>, ()> {
    let (input, use_superres) = if enable_superres {
        take_bool_bit(input)?
    } else {
        (input, false)
    };
    let (input, superres_denom) = if use_superres {
        let (input, coded_denom): (_, u32) = bit_parsers::take(SUPERRES_DENOM_BITS)(input)?;
        (input, coded_denom + SUPERRES_DENOM_MIN)
    } else {
        (input, SUPERRES_NUM)
    };
    upscaled_size.width = frame_size.width;
    frame_size.width = (upscaled_size.width * SUPERRES_NUM + (superres_denom / 2)) / superres_denom;
    Ok((input, ()))
}

fn compute_image_size(frame_size: Dimensions) -> (u32, u32) {
    let mi_cols = 2 * ((frame_size.width + 7) >> 3);
    let mi_rows = 2 * ((frame_size.height + 7) >> 3);
    (mi_cols, mi_rows)
}

fn read_interpolation_filter(input: BitInput) -> IResult<BitInput, ()> {
    let (input, _is_filter_switchable) = take_bool_bit(input)?;
    Ok((input, ()))
}

fn init_non_coeff_cdfs(input: BitInput) -> IResult<BitInput, ()> {
    // We don't care about this
    Ok((input, ()))
}

fn setup_past_independence(input: BitInput) -> IResult<BitInput, ()> {
    // We don't care about this
    Ok((input, ()))
}

fn load_cdfs(input: BitInput) -> IResult<BitInput, ()> {
    // We don't care about this
    Ok((input, ()))
}

fn load_previous(input: BitInput) -> IResult<BitInput, ()> {
    // We don't care about this
    Ok((input, ()))
}

fn motion_field_estimation(input: BitInput) -> IResult<BitInput, ()> {
    // We don't care about this
    Ok((input, ()))
}

fn tile_info(
    input: BitInput,
    use_128x128_superblock: bool,
    mi_cols: u32,
    mi_rows: u32,
) -> IResult<BitInput, ()> {
    let sb_cols = if use_128x128_superblock {
        (mi_cols + 31) >> 5
    } else {
        (mi_cols + 15) >> 4
    };
    let sb_rows = if use_128x128_superblock {
        (mi_rows + 31) >> 5
    } else {
        (mi_rows + 15) >> 4
    };
    let sb_shift = if use_128x128_superblock { 5 } else { 4 };
    let sb_size = sb_shift + 2;
    let max_tile_width_sb = MAX_TILE_WIDTH >> sb_size;
    let max_tile_area_sb = MAX_TILE_AREA >> (2 * sb_size);
    let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
    let max_log2_tile_cols = tile_log2(1, min(sb_cols, MAX_TILE_COLS));
    let max_log2_tile_rows = tile_log2(1, min(sb_rows, MAX_TILE_ROWS));
    let min_log2_tiles = max(
        min_log2_tile_cols,
        tile_log2(max_tile_area_sb, sb_rows * sb_cols),
    );
    let mut tile_rows = 0;
    let mut tile_cols = 0;

    let (mut input, uniform_tile_spacing_flag) = take_bool_bit(input)?;
    if uniform_tile_spacing_flag {
        let mut tile_cols_log2 = min_log2_tile_cols;
        while tile_cols_log2 < max_log2_tile_cols {
            let (inner_input, increment_tile_cols_log2) = take_bool_bit(input)?;
            input = inner_input;
            if increment_tile_cols_log2 {
                tile_cols_log2 += 2;
            } else {
                break;
            }
        }
        let tile_width_sb = (sb_cols + (1 << tile_cols_log2) - 1) >> tile_cols_log2;
        for i in (0..sb_cols).step_by(tile_width_sb) {
            // don't care about MiRowStarts
            tile_cols = i;
        }

        let mut min_log2_tile_rows = max(min_log2_tiles - tile_cols_log2, 0);
        let tile_rows_log2 = min_log2_tile_rows;
        while tile_rows_log2 < min_log2_tile_rows {
            let (inner_input, increment_tile_rows_log2) = take_bool_bit(input)?;
            input = inner_input;
            if increment_tile_rows_log2 {
                tile_rows_log2 += 2;
            } else {
                break;
            }
        }
        let tile_height_sb = (sb_rows + (1 << tile_rows_log2) - 1) >> tile_rows_log2;
        for i in (0..sb_rows).step_by(tile_height_sb as usize) {
            // don't care about MiRowStarts
            tile_rows = i;
        }
    } else {
        let mut widest_tile_sb = 0;
        let mut start_sb = 0;
        let mut i = 0;
        while start_sb < sb_cols {
            let max_width = min(sb_cols - start_sb, max_tile_width_sb);
            let (inner_input, width_in_sbs_minus_1) = ns(input, max_width as usize)?;
            input = inner_input;
            let size_sb = width_in_sbs_minus_1 + 1;
            start_sb += size_sb as u32;
            i += 1;
        }
        tile_cols = i;

        let mut start_sb = 0;
        let mut i = 0;
        let max_tile_height_sb = max(max_tile_area_sb / widest_tile_sb, 1);
        while start_sb < sb_rows {
            let max_height = min(sb_rows - start_sb, max_tile_height_sb);
            let (inner_input, height_in_sbs_minus_1) = ns(input, max_height as usize)?;
            input = inner_input;
            let size_sb = height_in_sbs_minus_1 + 1;
            start_sb += size_sb as u32;
            i += 1;
        }
        tile_rows = i;
    }

    let tile_cols_log2 = tile_log2(1, tile_cols);
    let tile_rows_log2 = tile_log2(1, tile_rows);
    let input = if tile_cols_log2 > 0 || tile_rows_log2 > 0 {
        let (input, context_update_tile_id) =
            bit_parsers::take(tile_rows_log2 + tile_cols_log2)(input)?;
        let (input, tile_size_bytes_minus_1): (_, u8) = bit_parsers::take(2)(input)?;
        input
    } else {
        input
    };

    Ok((input, ()))
}

/// Returns the smallest value for `k` such that `blk_size << k` is greater than
/// or equal to target.
///
/// There's probably a branchless way to do this,
/// but I copied what is in the spec.
fn tile_log2<T: PrimInt>(blk_size: T, target: T) -> T {
    let mut k = 0;
    while (blk_size << k) < target {
        k += 1;
    }
    k
}

fn quantization_params(
    input: BitInput,
    num_planes: u8,
    separate_uv_delta_q: bool,
) -> IResult<BitInput, QuantizationParams> {
    let (input, base_q_idx) = bit_parsers::take(8usize)(input)?;
    let (input, deltaq_y_dc) = read_delta_q(input)?;
    let (input, deltaq_u_dc, deltaq_u_ac, deltaq_v_dc, deltaq_v_ac) = if num_planes > 1 {
        let (input, diff_uv_delta) = if separate_uv_delta_q {
            take_bool_bit(input)?
        } else {
            (input, false)
        };
        let (input, deltaq_u_dc) = read_delta_q(input)?;
        let (input, deltaq_u_ac) = read_delta_q(input)?;
        let (input, deltaq_v_dc, deltaq_v_ac) = if diff_uv_delta {
            let (input, deltaq_v_dc) = read_delta_q(input)?;
            let (input, deltaq_v_ac) = read_delta_q(input)?;
            (input, deltaq_v_dc, deltaq_v_ac)
        } else {
            (input, deltaq_u_dc, deltaq_u_ac)
        };
    } else {
        (input, 0, 0, 0, 0)
    };
    let (input, using_qmatrix) = take_bool_bit(input)?;
    let input = if using_qmatrix {
        let (input, _qm_y): (_, u8) = bit_parsers::take(4usize)(input)?;
        let (input, _qm_u): (_, u8) = bit_parsers::take(4usize)(input)?;
        let (input, _qm_v): (_, u8) = if separate_uv_delta_q {
            bit_parsers::take(4usize)(input)?
        } else {
            (input, _qm_u)
        };
        input
    } else {
        input
    };

    Ok((input, QuantizationParams {
        base_q_idx,
        deltaq_y_dc,
        deltaq_u_ac,
        deltaq_u_dc,
        deltaq_v_ac,
        deltaq_v_dc,
    }))
}

fn read_delta_q(input: BitInput) -> IResult<BitInput, i64> {
    let (input, delta_coded) = take_bool_bit(input)?;
    if delta_coded {
        su(input, 1 + 6)
    } else {
        Ok((input, 0))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct QuantizationParams {
    pub base_q_idx: u8,
    pub deltaq_y_dc: i64,
    pub deltaq_u_dc: i64,
    pub deltaq_u_ac: i64,
    pub deltaq_v_dc: i64,
    pub deltaq_v_ac: i64,
}

fn segmentation_params(
    input: BitInput,
    primary_ref_frame: u8,
) -> IResult<BitInput, SegmentationData> {
    let mut segmentation_data: SegmentationData = Default::default();
    let (input, segmentation_enabled) = take_bool_bit(input)?;
    let input = if segmentation_enabled {
        let (input, segmentation_update_data) = if primary_ref_frame == PRIMARY_REF_NONE {
            (input, true)
        } else {
            let (input, segmentation_update_map) = take_bool_bit(input)?;
            let input = if segmentation_update_map {
                let (input, _segmentation_temporal_update) = take_bool_bit(input)?;
                input
            } else {
                input
            };
            take_bool_bit(input)?
        };
        if segmentation_update_data {
            let mut input = input;
            for i in 0..MAX_SEGMENTS {
                for j in 0..SEG_LVL_MAX {
                    let (inner_input, feature_enabled) = take_bool_bit(input)?;
                    input = if feature_enabled {
                        let bits_to_read = SEGMENTATION_FEATURE_BITS[j] as usize;
                        let limit = SEGMENTATION_FEATURE_MAX[j] as i16;
                        let (inner_input, feature_value) = if SEGMENTATION_FEATURE_SIGNED[j] {
                            let (input, value) = su(inner_input, 1 + bits_to_read)?;
                            (input, clamp(value as i16, -limit, limit))
                        } else {
                            let (input, value) = bit_parsers::take(bits_to_read)(inner_input)?;
                            (input, clamp(value, 0, limit))
                        };
                        segmentation_data[i][j] = Some(feature_value);
                        inner_input
                    } else {
                        inner_input
                    };
                }
            }
            input
        } else {
            input
        }
    } else {
        input
    };

    // The rest of the stuff in this method doesn't read any input, so return
    Ok((input, segmentation_data))
}

fn delta_q_params(input: BitInput, base_q_idx: u8) -> IResult<BitInput, bool> {
    let (input, delta_q_present) = if base_q_idx > 0 {
        take_bool_bit(input)?
    } else {
        (input, false)
    };
    let (input, _delta_q_res): (_, u8) = if delta_q_present {
        bit_parsers::take(2usize)(input)?
    } else {
        (input, 0)
    };
    Ok((input, delta_q_present))
}

fn delta_lf_params(
    input: BitInput,
    delta_q_present: bool,
    allow_intrabc: bool,
) -> IResult<BitInput, ()> {
    let input = if delta_q_present {
        let (input, delta_lf_present) = if !allow_intrabc {
            take_bool_bit(input)?
        } else {
            (input, false)
        };
        if delta_lf_present {
            let (input, delta_lf_res): (_, u8) = bit_parsers::take(2usize)(input)?;
            let (input, delta_lf_multi) = take_bool_bit(input)?;
            input
        } else {
            input
        }
    } else {
        input
    };
    Ok((input, ()))
}

fn init_coeff_cdfs(input: BitInput) -> IResult<BitInput, ()> {
    // We don't care about this
    Ok((input, ()))
}

fn load_previous_segment_ids(input: BitInput) -> IResult<BitInput, ()> {
    // We don't care about this
    Ok((input, ()))
}

fn loop_filter_params(
    input: BitInput,
    coded_lossless: bool,
    allow_intrabc: bool,
    num_planes: u8,
) -> IResult<BitInput, ()> {
    if coded_lossless || allow_intrabc {
        return Ok((input, ()));
    }

    let (input, loop_filter_l0): (_, u8) = bit_parsers::take(6usize)(input)?;
    let (input, loop_filter_l1): (_, u8) = bit_parsers::take(6usize)(input)?;
    let input = if num_planes > 1 && (loop_filter_l0 > 0 || loop_filter_l1 > 0) {
        let (input, loop_filter_l2): (_, u8) = bit_parsers::take(6usize)(input)?;
        let (input, loop_filter_l3): (_, u8) = bit_parsers::take(6usize)(input)?;
        input
    } else {
        input
    };
    let (input, loop_filter_sharpness): (_, u8) = bit_parsers::take(3usize)(input)?;
    let (input, loop_filter_delta_enabled) = take_bool_bit(input)?;
    let input = if loop_filter_delta_enabled {
        let (mut input, loop_filter_delta_update) = take_bool_bit(input)?;
        if loop_filter_delta_update {
            for i in 0..TOTAL_REFS_PER_FRAME {
                let (inner_input, update_ref_delta) = take_bool_bit(input)?;
                input = if update_ref_delta {
                    let (inner_input, loop_filter_ref_delta) = su(inner_input, 1 + 6)?;
                    inner_input
                } else {
                    inner_input
                };
            }
            for i in 0..2 {
                let (inner_input, update_mode_delta) = take_bool_bit(input)?;
                input = if update_mode_delta {
                    let (inner_input, loop_filter_mode_delta) = su(inner_input, 1 + 6)?;
                    inner_input
                } else {
                    inner_input
                };
            }
            input
        } else {
            input
        }
    } else {
        input
    };

    Ok((input, ()))
}

fn cdef_params(
    input: BitInput,
    coded_lossless: bool,
    allow_intrabc: bool,
    enable_cdef: bool,
    num_planes: u8,
) -> IResult<BitInput, ()> {
    if coded_lossless || allow_intrabc || !enable_cdef {
        return Ok((input, ()));
    }

    let (input, cdef_damping_minus_1): (_, u8) = bit_parsers::take(2usize)(input)?;
    let (mut input, cdef_bits): (_, u8) = bit_parsers::take(2usize)(input)?;
    for _ in 0..(1 << cdef_bits) {
        let (inner_input, cdef_y_pri_str): (_, u8) = bit_parsers::take(4usize)(input)?;
        let (inner_input, cdef_y_sec_str): (_, u8) = bit_parsers::take(2usize)(inner_input)?;
        input = if num_planes > 1 {
            let (inner_input, cdef_uv_pri_str): (_, u8) = bit_parsers::take(4usize)(inner_input)?;
            let (inner_input, cdef_uv_sec_str): (_, u8) = bit_parsers::take(2usize)(inner_input)?;
            inner_input
        } else {
            inner_input
        }
    }

    Ok((input, ()))
}

#[allow(clippy::fn_params_excessive_bools)]
fn lr_params<'a>(
    input: BitInput<'a>,
    all_lossless: bool,
    allow_intrabc: bool,
    enable_restoration: bool,
    use_128x128_superblock: bool,
    num_planes: u8,
    subsampling: (u8, u8),
) -> IResult<BitInput<'a>, ()> {
    if all_lossless || allow_intrabc || !enable_restoration {
        return Ok((input, ()));
    }

    let mut input = input;
    let mut uses_lr = false;
    let mut uses_chroma_lr = false;
    for i in 0..num_planes {
        let (inner_input, lr_type): (_, u8) = bit_parsers::take(2usize)(input)?;
        if lr_type != RESTORE_NONE {
            uses_lr = true;
            if i > 0 {
                uses_chroma_lr = true;
            }
        }
        input = inner_input;
    }

    let input = if uses_lr {
        let input = if use_128x128_superblock {
            let (input, lr_unit_shift) = take_bool_bit(input)?;
            input
        } else {
            let (input, lr_unit_shift) = take_bool_bit(input)?;
            if lr_unit_shift {
                let (input, lr_unit_extra_shift) = take_bool_bit(input)?;
                input
            } else {
                input
            }
        };
        if subsampling.0 > 0 && subsampling.1 > 0 && uses_chroma_lr {
            let (input, lr_uv_shift) = take_bool_bit(input)?;
            input
        } else {
            input
        }
    } else {
        input
    };

    Ok((input, ()))
}

fn read_tx_mode(input: BitInput, coded_lossless: bool) -> IResult<BitInput, ()> {
    let input = if coded_lossless {
        input
    } else {
        let (input, _tx_mode_select) = take_bool_bit(input)?;
        input
    };
    Ok((input, ()))
}

fn frame_reference_mode(input: BitInput, frame_is_intra: bool) -> IResult<BitInput, bool> {
    Ok(if frame_is_intra {
        (input, false)
    } else {
        take_bool_bit(input)?
    })
}

fn skip_mode_params<'a>(
    input: BitInput<'a>,
    frame_is_intra: bool,
    reference_select: bool,
    order_hint_bits: usize,
    order_hint: u64,
    ref_order_hint: &'a [u64],
    ref_frame_idx: &'a [usize],
) -> IResult<BitInput<'a>, ()> {
    let mut skip_mode_allowed = false;
    let mut forward_hint = -1;
    let mut backward_hint = -1;
    let mut second_forward_hint = -1;
    if frame_is_intra || !reference_select || order_hint_bits == 0 {
        skip_mode_allowed = false;
    } else {
        let mut forward_idx = -1;
        let mut backward_idx = -1;
        for i in 0..(REFS_PER_FRAME as isize) {
            let ref_hint = ref_order_hint[ref_frame_idx[i as usize]];
            if get_relative_dist(ref_hint as i64, order_hint as i64, order_hint_bits) < 0 {
                if forward_idx < 0
                    || get_relative_dist(ref_hint as i64, forward_hint, order_hint_bits) > 0
                {
                    forward_idx = i;
                    forward_hint = ref_hint as i64;
                }
            } else if get_relative_dist(ref_hint as i64, order_hint as i64, order_hint_bits) > 0
                && (backward_idx < 0
                    || get_relative_dist(ref_hint as i64, backward_hint, order_hint_bits) < 0)
            {
                backward_idx = i;
                backward_hint = ref_hint as i64;
            }
        }

        if forward_idx < 0 {
            skip_mode_allowed = false;
        } else if backward_idx >= 0 {
            skip_mode_allowed = true;
        } else {
            let mut second_forward_idx = -1;
            for i in 0..(REFS_PER_FRAME as isize) {
                let ref_hint = ref_order_hint[ref_frame_idx[i as usize]];
                if get_relative_dist(ref_hint as i64, forward_hint, order_hint_bits) < 0
                    && (second_forward_idx < 0
                        || get_relative_dist(ref_hint as i64, second_forward_hint, order_hint_bits)
                            > 0)
                {
                    second_forward_idx = i;
                    second_forward_hint = ref_hint as i64;
                }
            }

            if second_forward_idx < 0 {
                skip_mode_allowed = false;
            } else {
                skip_mode_allowed = true;
            }
        }
    }

    let (input, skip_mode_present) = if skip_mode_allowed {
        take_bool_bit(input)?
    } else {
        (input, false)
    };

    Ok((input, ()))
}

fn get_relative_dist(a: i64, b: i64, order_hint_bits: usize) -> i64 {
    if order_hint_bits == 0 {
        return 0;
    }

    let diff = a - b;
    let m = 1 << (order_hint_bits - 1);
    (diff & (m - 1)) - (diff & m)
}

fn global_motion_params(input: BitInput, frame_is_intra: bool) -> IResult<BitInput, ()> {
    if frame_is_intra {
        return Ok((input, ()));
    }

    let mut outer_input = input;
    for _ in (RefType::Last as u8)..=(RefType::Altref as u8) {
        let input = outer_input;
        let (input, is_global) = take_bool_bit(input)?;
        outer_input = if is_global {
            let (input, is_rot_zoom) = take_bool_bit(input)?;
            if is_rot_zoom {
                input
            } else {
                let (input, is_translation) = take_bool_bit(input)?;
                input
            }
        } else {
            input
        };
    }

    Ok((outer_input, ()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum RefType {
    Intra = 0,
    Last = 1,
    Last2 = 2,
    Last3 = 3,
    Golden = 4,
    Bwdref = 5,
    Altref2 = 6,
    Altref = 7,
}

fn get_qindex(
    ignore_delta_q: bool,
    segment_id: usize,
    base_q_idx: u8,
    current_q_index: Option<u8>,
    segmentation_enabled: bool,
    feature_data: &SegmentationData,
) -> u8 {
    if seg_feature_active_idx(
        segment_id,
        SEG_LVL_ALT_Q,
        segmentation_enabled,
        feature_data,
    ) {
        let data = feature_data[segment_id][SEG_LVL_ALT_Q].unwrap();
        let mut qindex = base_q_idx as i16 + data;
        if !ignore_delta_q && current_q_index.is_some() {
            qindex = current_q_index.unwrap() as i16 + data;
        }
        return clamp(qindex, 0, 255) as u8;
    } else if !ignore_delta_q && current_q_index.is_some() {
        return current_q_index.unwrap();
    }
    return base_q_idx;
}

#[inline(always)]
fn seg_feature_active_idx(
    segment_id: usize,
    feature: usize,
    segmentation_enabled: bool,
    feature_data: &SegmentationData,
) -> bool {
    segmentation_enabled && feature_data[segment_id][SEG_LVL_ALT_Q].is_some()
}
