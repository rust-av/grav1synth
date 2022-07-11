use std::cmp::{max, min};

use nom::{
    bits::{bits, complete as bit_parsers},
    error::{context, VerboseError},
    IResult,
};
use num_enum::TryFromPrimitive;
use num_traits::{clamp, PrimInt};

use super::{
    grain::{film_grain_params, FilmGrainHeader},
    obu::ObuHeader,
    sequence::{SELECT_INTEGER_MV, SELECT_SCREEN_CONTENT_TOOLS},
    util::{ns, su, take_bool_bit, BitInput},
    BitstreamParser,
};

pub const REFS_PER_FRAME: usize = 7;
const TOTAL_REFS_PER_FRAME: usize = 8;
pub const NUM_REF_FRAMES: usize = 8;
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

const INTERP_FILTER_SWITCHABLE: u8 = 4;
const MAX_LOOP_FILTER: u8 = 63;
const RESTORE_NONE: u8 = 0;

#[derive(Debug, Clone)]
pub struct FrameHeader {
    pub show_frame: bool,
    pub show_existing_frame: bool,
    pub film_grain_params: FilmGrainHeader,
    pub tile_info: TileInfo,
}

impl<const WRITE: bool> BitstreamParser<WRITE> {
    pub fn parse_frame_obu<'a>(
        &mut self,
        input: &'a [u8],
        obu_header: ObuHeader,
    ) -> IResult<&'a [u8], Option<FrameHeader>, VerboseError<&'a [u8]>> {
        let input_len = input.len();
        let (input, frame_header) = context("Failed parsing frame header", |input| {
            self.parse_frame_header(input, obu_header)
        })(input)?;
        let ref_frame_header = frame_header
            .clone()
            .or_else(|| self.previous_frame_header.clone())
            .unwrap();
        // A reminder that obu size is in bytes
        let tile_group_obu_size = self.size - (input_len - input.len());
        let (input, _) = context("Failed parsing tile group obu", |input| {
            self.parse_tile_group_obu(input, tile_group_obu_size, ref_frame_header.tile_info)
        })(input)?;
        Ok((input, frame_header))
    }

    /// This will return `None` for a show-existing frame. We don't need to
    /// apply film grain params to those packets, because they are inherited
    /// from the ref frame.
    ///
    /// I wish we didn't have to parse the whole frame header,
    /// but the film grain params are of course the very last item,
    /// and we don't know how many bits precede it, so we have to parse
    /// THE WHOLE THING before we get the film grain params.
    pub fn parse_frame_header<'a>(
        &mut self,
        input: &'a [u8],
        obu_header: ObuHeader,
    ) -> IResult<&'a [u8], Option<FrameHeader>, VerboseError<&'a [u8]>> {
        if self.seen_frame_header {
            return Ok((input, None));
        }

        self.seen_frame_header = true;
        bits(|input| {
            let (input, header) = self.uncompressed_header(input, obu_header)?;
            if header.show_existing_frame {
                let (input, _) = decode_frame_wrapup(input)?;
                self.seen_frame_header = false;
                Ok((input, header.show_frame.then(|| header)))
            } else {
                self.seen_frame_header = true;
                Ok((input, header.show_frame.then(|| header)))
            }
        })(input)
    }

    #[allow(clippy::cognitive_complexity)]
    #[allow(clippy::too_many_lines)]
    fn uncompressed_header<'a>(
        &mut self,
        input: BitInput<'a>,
        obu_headers: ObuHeader,
    ) -> IResult<BitInput<'a>, FrameHeader, VerboseError<BitInput<'a>>> {
        let sequence_header = self.sequence_header.as_ref().unwrap();
        let id_len = sequence_header.frame_id_numbers_present.then(|| {
            sequence_header.additional_frame_id_len_minus_1
                + sequence_header.delta_frame_id_len_minus_2
                + 3
        });

        let (
            input,
            frame_type,
            show_frame,
            showable_frame,
            show_existing_frame,
            error_resilient_mode,
        ) = if sequence_header.reduced_still_picture_header {
            (input, FrameType::Inter, true, true, false, false)
        } else {
            let (input, show_existing_frame) = take_bool_bit(input)?;
            if show_existing_frame {
                let (input, _frame_to_show_map_idx): (_, u8) = bit_parsers::take(3usize)(input)?;
                let input = if let Some(id_len) = id_len {
                    let (input, _display_frame_id): (_, u64) = bit_parsers::take(id_len)(input)?;
                    input
                } else {
                    input
                };
                return Ok((input, FrameHeader {
                    show_frame: true,
                    show_existing_frame,
                    film_grain_params: FilmGrainHeader::CopyRefFrame,
                    tile_info: self.previous_frame_header.as_ref().unwrap().tile_info,
                }));
            };
            let (input, frame_type): (_, u8) = bit_parsers::take(2usize)(input)?;
            let frame_type = FrameType::try_from(frame_type).unwrap();
            let (input, show_frame) = take_bool_bit(input)?;
            let input = if show_frame
                && sequence_header.decoder_model_info.is_some()
                && !sequence_header
                    .timing_info
                    .map_or(false, |ti| ti.equal_picture_interval)
            {
                temporal_point_info(
                    input,
                    sequence_header
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
                (input, frame_type != FrameType::Key)
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

        if frame_type == FrameType::Key && show_frame {
            for i in 0..NUM_REF_FRAMES {
                self.big_ref_valid[i] = false;
                self.big_ref_order_hint[i] = 0;
            }
            for i in 0..REFS_PER_FRAME {
                self.big_order_hints[i + RefType::Last as usize] = 0;
            }
        }

        let (input, disable_cdf_update) = take_bool_bit(input)?;
        let (input, allow_screen_content_tools) =
            if sequence_header.force_screen_content_tools == SELECT_SCREEN_CONTENT_TOOLS {
                take_bool_bit(input)?
            } else {
                (input, sequence_header.force_screen_content_tools == 1)
            };
        let input = if allow_screen_content_tools
            && sequence_header.force_integer_mv == SELECT_INTEGER_MV
        {
            take_bool_bit(input)?.0
        } else {
            input
        };
        let input = if sequence_header.frame_id_numbers_present {
            let (input, _current_frame_id): (_, usize) = bit_parsers::take(id_len.unwrap())(input)?;
            input
        } else {
            input
        };
        let (input, frame_size_override_flag) = if frame_type == FrameType::Switch {
            (input, true)
        } else if sequence_header.reduced_still_picture_header {
            (input, false)
        } else {
            take_bool_bit(input)?
        };
        let (input, order_hint): (_, u64) =
            bit_parsers::take(sequence_header.order_hint_bits)(input)?;
        let (input, primary_ref_frame) = if frame_type.is_intra() || error_resilient_mode {
            (input, PRIMARY_REF_NONE)
        } else {
            bit_parsers::take(3usize)(input)?
        };

        let mut input = input;
        if let Some(decoder_model_info) = sequence_header.decoder_model_info {
            let (inner_input, buffer_removal_time_present_flag) = take_bool_bit(input)?;
            if buffer_removal_time_present_flag {
                for op_num in 0..=sequence_header.operating_points_cnt_minus_1 {
                    if sequence_header.decoder_model_present_for_op[op_num] {
                        let op_pt_idc = sequence_header.operating_point_idc[op_num];
                        let temporal_id = obu_headers.extension.map_or(0, |ext| ext.temporal_id);
                        let spatial_id = obu_headers.extension.map_or(0, |ext| ext.spatial_id);
                        let in_temporal_layer = (op_pt_idc >> temporal_id) & 1 > 0;
                        let in_spatial_layer = (op_pt_idc >> (spatial_id + 8)) & 1 > 0;
                        if op_pt_idc == 0 || (in_temporal_layer && in_spatial_layer) {
                            let n = decoder_model_info.buffer_removal_time_length_minus_1 + 1;
                            let (inner_input, _buffer_removal_time): (_, u64) =
                                bit_parsers::take(n)(inner_input)?;
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
                bit_parsers::take(8usize)(input)?
            };

        let mut input = input;
        if (!frame_type.is_intra() || refresh_frame_flags != REFRESH_ALL_FRAMES)
            && error_resilient_mode
            && sequence_header.enable_order_hint()
        {
            for i in 0..NUM_REF_FRAMES {
                let (inner_input, cur_ref_order_hint): (_, u64) =
                    bit_parsers::take(sequence_header.order_hint_bits)(input)?;
                self.big_ref_order_hint[i] = self.ref_order_hint[i];
                self.ref_order_hint[i] = cur_ref_order_hint;
                if self.ref_order_hint[i] != self.big_ref_order_hint[i] {
                    self.big_ref_valid[i] = false;
                }
                input = inner_input;
            }
        }

        let max_frame_size = Dimensions {
            width: sequence_header.max_frame_width_minus_1 + 1,
            height: sequence_header.max_frame_height_minus_1 + 1,
        };
        let (input, use_ref_frame_mvs, frame_size, upscaled_size) = if frame_type.is_intra() {
            let (input, frame_size) = frame_size(
                input,
                frame_size_override_flag,
                sequence_header.enable_superres,
                sequence_header.frame_width_bits_minus_1 + 1,
                sequence_header.frame_height_bits_minus_1 + 1,
                max_frame_size,
            )?;
            let mut upscaled_size = frame_size;
            let (input, _render_size) = render_size(input, frame_size, &mut upscaled_size)?;
            (
                if allow_screen_content_tools && upscaled_size.width == frame_size.width {
                    let (input, allow_intrabc_inner) = take_bool_bit(input)?;
                    allow_intrabc = allow_intrabc_inner;
                    input
                } else {
                    input
                },
                false,
                frame_size,
                upscaled_size,
            )
        } else {
            let (mut input, frame_refs_short_signaling) = if sequence_header.enable_order_hint() {
                let (input, frame_refs_short_signaling) = take_bool_bit(input)?;
                if frame_refs_short_signaling {
                    let (input, _last_frame_idx): (_, u8) = bit_parsers::take(3usize)(input)?;
                    let (input, _gold_frame_idx): (_, u8) = bit_parsers::take(3usize)(input)?;
                    let (input, _) = set_frame_refs(input)?;
                    (input, frame_refs_short_signaling)
                } else {
                    (input, frame_refs_short_signaling)
                }
            } else {
                (input, false)
            };

            for ref_frame_idx in &mut self.ref_frame_idx {
                if frame_refs_short_signaling {
                    *ref_frame_idx = 0;
                } else {
                    let (inner_input, this_ref_frame_idx) = bit_parsers::take(3usize)(input)?;
                    input = inner_input;
                    *ref_frame_idx = this_ref_frame_idx;
                    if sequence_header.frame_id_numbers_present {
                        let n = sequence_header.delta_frame_id_len_minus_2 + 2;
                        let (inner_input, _delta_frame_id_minus_1): (_, u64) =
                            bit_parsers::take(n)(input)?;
                        input = inner_input;
                    }
                }
            }
            let (input, frame_size, upscaled_size) =
                if frame_size_override_flag && !error_resilient_mode {
                    let mut frame_size = max_frame_size;
                    let mut upscaled_size = frame_size;
                    let (input, frame_size) = frame_size_with_refs(
                        input,
                        sequence_header.enable_superres,
                        frame_size_override_flag,
                        sequence_header.frame_width_bits_minus_1 + 1,
                        sequence_header.frame_height_bits_minus_1 + 1,
                        max_frame_size,
                        &mut frame_size,
                        &mut upscaled_size,
                    )?;
                    (input, frame_size, upscaled_size)
                } else {
                    let (input, frame_size) = frame_size(
                        input,
                        frame_size_override_flag,
                        sequence_header.enable_superres,
                        sequence_header.frame_width_bits_minus_1 + 1,
                        sequence_header.frame_height_bits_minus_1 + 1,
                        max_frame_size,
                    )?;
                    let mut upscaled_size = frame_size;
                    let (input, _render_size) = render_size(input, frame_size, &mut upscaled_size)?;
                    (input, frame_size, upscaled_size)
                };
            let (input, _allow_high_precision_mv) = if sequence_header.force_integer_mv == 1 {
                (input, false)
            } else {
                take_bool_bit(input)?
            };
            let (input, _) = read_interpolation_filter(input)?;
            let (input, _is_motion_mode_switchable) = take_bool_bit(input)?;
            let (input, use_ref_frame_mvs) =
                if error_resilient_mode || !sequence_header.enable_ref_frame_mvs {
                    (input, false)
                } else {
                    take_bool_bit(input)?
                };
            for i in 0..REFS_PER_FRAME {
                let ref_frame = RefType::Last as usize + i;
                let hint = self.big_ref_order_hint[self.ref_frame_idx[i]];
                self.big_order_hints[ref_frame] = hint;
                // don't think we care about ref frame sign bias
            }
            (input, use_ref_frame_mvs, frame_size, upscaled_size)
        };
        let (mi_cols, mi_rows) = compute_image_size(frame_size);

        let (input, _disable_frame_end_update_cdf) =
            if sequence_header.reduced_still_picture_header || disable_cdf_update {
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
        let (input, tile_info) = tile_info(
            input,
            sequence_header.use_128x128_superblock,
            mi_cols,
            mi_rows,
        )?;
        let (input, q_params) = quantization_params(
            input,
            sequence_header.color_config.num_planes,
            sequence_header.color_config.separate_uv_delta_q,
        )?;
        let (input, segmentation_data) = segmentation_params(input, primary_ref_frame)?;
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
                None,
                segmentation_data.as_ref(),
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
            sequence_header.color_config.num_planes,
        )?;
        let (input, _) = cdef_params(
            input,
            coded_lossless,
            allow_intrabc,
            sequence_header.enable_cdef,
            sequence_header.color_config.num_planes,
        )?;
        let (input, _) = lr_params(
            input,
            all_losslesss,
            allow_intrabc,
            sequence_header.enable_restoration,
            sequence_header.use_128x128_superblock,
            sequence_header.color_config.num_planes,
            sequence_header.color_config.subsampling,
        )?;
        let (input, _) = read_tx_mode(input, coded_lossless)?;
        let (input, reference_select) = frame_reference_mode(input, frame_type.is_intra())?;
        let (input, _) = skip_mode_params(
            input,
            frame_type.is_intra(),
            reference_select,
            sequence_header.order_hint_bits,
            order_hint,
            &self.big_ref_order_hint,
            &self.ref_frame_idx,
        )?;
        let (input, _allow_warped_motion) = if frame_type.is_intra()
            || error_resilient_mode
            || !sequence_header.enable_warped_motion
        {
            (input, false)
        } else {
            take_bool_bit(input)?
        };
        let (input, _reduced_tx_set) = take_bool_bit(input)?;
        let (input, _) = global_motion_params(input, frame_type.is_intra())?;
        let (input, film_grain_params) = film_grain_params(
            input,
            sequence_header.film_grain_params_present,
            show_frame,
            showable_frame,
            frame_type,
            sequence_header.color_config.num_planes == 1,
            sequence_header.color_config.subsampling,
        )?;

        for i in 0..NUM_REF_FRAMES {
            if (refresh_frame_flags >> i) & 1 == 1 {
                self.big_ref_valid[i] = true;
                self.big_ref_order_hint[i] = order_hint;
            }
        }

        Ok((input, FrameHeader {
            show_frame,
            show_existing_frame,
            film_grain_params,
            tile_info,
        }))
    }
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
    #[must_use]
    pub fn is_intra(self) -> bool {
        self == FrameType::Key || self == FrameType::IntraOnly
    }
}

#[inline(always)]
#[allow(clippy::unnecessary_wraps)]
const fn decode_frame_wrapup(input: BitInput) -> IResult<BitInput, (), VerboseError<BitInput>> {
    // I don't believe this actually parses anything
    // or does anything relevant to us...
    Ok((input, ()))
}

fn temporal_point_info(
    input: BitInput,
    frame_presentation_time_length: usize,
) -> IResult<BitInput, (), VerboseError<BitInput>> {
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
) -> IResult<BitInput, Dimensions, VerboseError<BitInput>> {
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

fn render_size<'a, 'b>(
    input: BitInput<'a>,
    frame_size: Dimensions,
    upscaled_size: &'b mut Dimensions,
) -> IResult<BitInput<'a>, Dimensions, VerboseError<BitInput<'a>>> {
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

#[inline(always)]
#[allow(clippy::unnecessary_wraps)]
const fn set_frame_refs(input: BitInput) -> IResult<BitInput, (), VerboseError<BitInput>> {
    // Does nothing that we care about
    Ok((input, ()))
}

#[allow(clippy::too_many_arguments)]
fn frame_size_with_refs<'a, 'b>(
    input: BitInput<'a>,
    enable_superres: bool,
    frame_size_override: bool,
    frame_width_bits: usize,
    frame_height_bits: usize,
    max_frame_size: Dimensions,
    ref_frame_size: &'b mut Dimensions,
    ref_upscaled_size: &'b mut Dimensions,
) -> IResult<BitInput<'a>, Dimensions, VerboseError<BitInput<'a>>> {
    let mut found_ref = false;
    let mut input = input;
    for _ in 0..REFS_PER_FRAME {
        let (inner_input, found_this_ref) = take_bool_bit(input)?;
        input = inner_input;
        if found_this_ref {
            found_ref = true;
            // We don't actually care about the changes to frame size. But if we did, we'd
            // have to do things here.
            break;
        }
    }
    let (input, frame_size) = if found_ref {
        let (input, _) =
            superres_params(input, enable_superres, ref_frame_size, ref_upscaled_size)?;
        (input, *ref_frame_size)
    } else {
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
    };
    Ok((input, frame_size))
}

fn superres_params<'a, 'b>(
    input: BitInput<'a>,
    enable_superres: bool,
    frame_size: &'b mut Dimensions,
    upscaled_size: &'b mut Dimensions,
) -> IResult<BitInput<'a>, (), VerboseError<BitInput<'a>>> {
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

const fn compute_image_size(frame_size: Dimensions) -> (u32, u32) {
    let mi_cols = 2 * ((frame_size.width + 7) >> 3u8);
    let mi_rows = 2 * ((frame_size.height + 7) >> 3u8);
    (mi_cols, mi_rows)
}

fn read_interpolation_filter(input: BitInput) -> IResult<BitInput, (), VerboseError<BitInput>> {
    let (input, is_filter_switchable) = take_bool_bit(input)?;
    let (input, _interpolation_filter) = if is_filter_switchable {
        (input, INTERP_FILTER_SWITCHABLE)
    } else {
        bit_parsers::take(2usize)(input)?
    };
    Ok((input, ()))
}

#[inline(always)]
#[allow(clippy::unnecessary_wraps)]
const fn init_non_coeff_cdfs(input: BitInput) -> IResult<BitInput, (), VerboseError<BitInput>> {
    // We don't care about this
    Ok((input, ()))
}

#[inline(always)]
#[allow(clippy::unnecessary_wraps)]
const fn setup_past_independence(input: BitInput) -> IResult<BitInput, (), VerboseError<BitInput>> {
    // We don't care about this
    Ok((input, ()))
}

#[inline(always)]
#[allow(clippy::unnecessary_wraps)]
const fn load_cdfs(input: BitInput) -> IResult<BitInput, (), VerboseError<BitInput>> {
    // We don't care about this
    Ok((input, ()))
}

#[inline(always)]
#[allow(clippy::unnecessary_wraps)]
const fn load_previous(input: BitInput) -> IResult<BitInput, (), VerboseError<BitInput>> {
    // We don't care about this
    Ok((input, ()))
}

#[inline(always)]
#[allow(clippy::unnecessary_wraps)]
const fn motion_field_estimation(input: BitInput) -> IResult<BitInput, (), VerboseError<BitInput>> {
    // We don't care about this
    Ok((input, ()))
}

#[allow(clippy::too_many_lines)]
fn tile_info(
    input: BitInput,
    use_128x128_superblock: bool,
    mi_cols: u32,
    mi_rows: u32,
) -> IResult<BitInput, TileInfo, VerboseError<BitInput>> {
    let sb_cols = if use_128x128_superblock {
        (mi_cols + 31) >> 5u8
    } else {
        (mi_cols + 15) >> 4u8
    };
    let sb_rows = if use_128x128_superblock {
        (mi_rows + 31) >> 5u8
    } else {
        (mi_rows + 15) >> 4u8
    };
    let sb_shift = if use_128x128_superblock { 5u8 } else { 4u8 };
    let sb_size = sb_shift + 2;
    let max_tile_width_sb = MAX_TILE_WIDTH >> sb_size;
    let max_tile_area_sb = MAX_TILE_AREA >> (2u8 * sb_size);
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
    let (tile_cols_log2, tile_rows_log2) = if uniform_tile_spacing_flag {
        let mut tile_cols_log2 = min_log2_tile_cols;
        while tile_cols_log2 < max_log2_tile_cols {
            let (inner_input, increment_tile_cols_log2) = take_bool_bit(input)?;
            input = inner_input;
            if increment_tile_cols_log2 {
                tile_cols_log2 += 1;
            } else {
                break;
            }
        }
        let tile_width_sb = (sb_cols + (1 << tile_cols_log2) - 1) >> tile_cols_log2;
        for i in (0..sb_cols).step_by(tile_width_sb as usize) {
            // don't care about MiRowStarts
            tile_cols = i + 1;
        }

        let min_log2_tile_rows = max(min_log2_tiles as i32 - tile_cols_log2 as i32, 0i32) as u32;
        let mut tile_rows_log2 = min_log2_tile_rows;
        while tile_rows_log2 < max_log2_tile_rows {
            let (inner_input, increment_tile_rows_log2) = take_bool_bit(input)?;
            input = inner_input;
            if increment_tile_rows_log2 {
                tile_rows_log2 += 1;
            } else {
                break;
            }
        }
        let tile_height_sb = (sb_rows + (1 << tile_rows_log2) - 1) >> tile_rows_log2;
        for i in (0..sb_rows).step_by(tile_height_sb as usize) {
            // don't care about MiRowStarts
            tile_rows = i + 1;
        }

        (tile_cols_log2, tile_rows_log2)
    } else {
        let mut widest_tile_sb = 0;
        let mut start_sb = 0;
        let mut i = 0;
        while start_sb < sb_cols {
            let max_width = min(sb_cols - start_sb, max_tile_width_sb);
            let (inner_input, width_in_sbs_minus_1) = ns(input, max_width as usize)?;
            input = inner_input;
            let size_sb = width_in_sbs_minus_1 + 1;
            widest_tile_sb = max(size_sb as u32, widest_tile_sb);
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

        let tile_cols_log2 = tile_log2(1, tile_cols);
        let tile_rows_log2 = tile_log2(1, tile_rows);

        (tile_cols_log2, tile_rows_log2)
    };
    assert!(tile_cols > 0);
    assert!(tile_rows > 0);

    let input = if tile_cols_log2 > 0 || tile_rows_log2 > 0 {
        let (input, _context_update_tile_id): (_, u64) =
            bit_parsers::take(tile_rows_log2 + tile_cols_log2)(input)?;
        let (input, _tile_size_bytes_minus_1): (_, u8) = bit_parsers::take(2usize)(input)?;
        input
    } else {
        input
    };

    Ok((input, TileInfo {
        tile_cols,
        tile_rows,
        tile_cols_log2,
        tile_rows_log2,
    }))
}

#[derive(Debug, Clone, Copy)]
pub struct TileInfo {
    pub tile_cols: u32,
    pub tile_rows: u32,
    pub tile_cols_log2: u32,
    pub tile_rows_log2: u32,
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
    T::from(k).unwrap()
}

fn quantization_params(
    input: BitInput,
    num_planes: u8,
    separate_uv_delta_q: bool,
) -> IResult<BitInput, QuantizationParams, VerboseError<BitInput>> {
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
        (input, deltaq_u_dc, deltaq_u_ac, deltaq_v_dc, deltaq_v_ac)
    } else {
        (input, 0, 0, 0, 0)
    };
    let (input, using_qmatrix) = take_bool_bit(input)?;
    let input = if using_qmatrix {
        let (input, _qm_y): (_, u8) = bit_parsers::take(4usize)(input)?;
        let (input, qm_u): (_, u8) = bit_parsers::take(4usize)(input)?;
        let (input, _qm_v): (_, u8) = if separate_uv_delta_q {
            bit_parsers::take(4usize)(input)?
        } else {
            (input, qm_u)
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

fn read_delta_q(input: BitInput) -> IResult<BitInput, i64, VerboseError<BitInput>> {
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
) -> IResult<BitInput, Option<SegmentationData>, VerboseError<BitInput>> {
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
            #[allow(clippy::needless_range_loop)]
            for i in 0..MAX_SEGMENTS {
                for j in 0..SEG_LVL_MAX {
                    let (inner_input, feature_enabled) = take_bool_bit(input)?;
                    input = if feature_enabled {
                        let bits_to_read = SEGMENTATION_FEATURE_BITS[j] as usize;
                        let limit = i16::from(SEGMENTATION_FEATURE_MAX[j]);
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
    Ok((input, segmentation_enabled.then(|| segmentation_data)))
}

fn delta_q_params(
    input: BitInput,
    base_q_idx: u8,
) -> IResult<BitInput, bool, VerboseError<BitInput>> {
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
) -> IResult<BitInput, (), VerboseError<BitInput>> {
    let input = if delta_q_present {
        let (input, delta_lf_present) = if allow_intrabc {
            (input, false)
        } else {
            take_bool_bit(input)?
        };
        if delta_lf_present {
            let (input, _delta_lf_res): (_, u8) = bit_parsers::take(2usize)(input)?;
            let (input, _delta_lf_multi) = take_bool_bit(input)?;
            input
        } else {
            input
        }
    } else {
        input
    };
    Ok((input, ()))
}

#[inline(always)]
#[allow(clippy::unnecessary_wraps)]
const fn init_coeff_cdfs(input: BitInput) -> IResult<BitInput, (), VerboseError<BitInput>> {
    // We don't care about this
    Ok((input, ()))
}

#[inline(always)]
#[allow(clippy::unnecessary_wraps)]
const fn load_previous_segment_ids(
    input: BitInput,
) -> IResult<BitInput, (), VerboseError<BitInput>> {
    // We don't care about this
    Ok((input, ()))
}

fn loop_filter_params(
    input: BitInput,
    coded_lossless: bool,
    allow_intrabc: bool,
    num_planes: u8,
) -> IResult<BitInput, (), VerboseError<BitInput>> {
    if coded_lossless || allow_intrabc {
        return Ok((input, ()));
    }

    let (input, loop_filter_l0): (_, u8) = bit_parsers::take(6usize)(input)?;
    let (input, loop_filter_l1): (_, u8) = bit_parsers::take(6usize)(input)?;
    let input = if num_planes > 1 && (loop_filter_l0 > 0 || loop_filter_l1 > 0) {
        let (input, _loop_filter_l2): (_, u8) = bit_parsers::take(6usize)(input)?;
        let (input, _loop_filter_l3): (_, u8) = bit_parsers::take(6usize)(input)?;
        input
    } else {
        input
    };
    let (input, _loop_filter_sharpness): (_, u8) = bit_parsers::take(3usize)(input)?;
    let (mut input, loop_filter_delta_enabled) = take_bool_bit(input)?;
    if loop_filter_delta_enabled {
        let (inner_input, loop_filter_delta_update) = take_bool_bit(input)?;
        input = inner_input;
        if loop_filter_delta_update {
            for _ in 0..TOTAL_REFS_PER_FRAME {
                let (inner_input, update_ref_delta) = take_bool_bit(input)?;
                input = if update_ref_delta {
                    let (inner_input, _loop_filter_ref_delta) = su(inner_input, 1 + 6)?;
                    inner_input
                } else {
                    inner_input
                };
            }
            for _ in 0..2u8 {
                let (inner_input, update_mode_delta) = take_bool_bit(input)?;
                input = if update_mode_delta {
                    let (inner_input, _loop_filter_mode_delta) = su(inner_input, 1 + 6)?;
                    inner_input
                } else {
                    inner_input
                };
            }
        }
    };

    Ok((input, ()))
}

fn cdef_params(
    input: BitInput,
    coded_lossless: bool,
    allow_intrabc: bool,
    enable_cdef: bool,
    num_planes: u8,
) -> IResult<BitInput, (), VerboseError<BitInput>> {
    if coded_lossless || allow_intrabc || !enable_cdef {
        return Ok((input, ()));
    }

    let (input, _cdef_damping_minus_3): (_, u8) = bit_parsers::take(2usize)(input)?;
    let (mut input, cdef_bits): (_, u8) = bit_parsers::take(2usize)(input)?;
    for _ in 0..(1usize << cdef_bits) {
        let (inner_input, _cdef_y_pri_str): (_, u8) = bit_parsers::take(4usize)(input)?;
        let (inner_input, _cdef_y_sec_str): (_, u8) = bit_parsers::take(2usize)(inner_input)?;
        input = if num_planes > 1 {
            let (inner_input, _cdef_uv_pri_str): (_, u8) = bit_parsers::take(4usize)(inner_input)?;
            let (inner_input, _cdef_uv_sec_str): (_, u8) = bit_parsers::take(2usize)(inner_input)?;
            inner_input
        } else {
            inner_input
        }
    }

    Ok((input, ()))
}

#[allow(clippy::fn_params_excessive_bools)]
fn lr_params(
    input: BitInput,
    all_lossless: bool,
    allow_intrabc: bool,
    enable_restoration: bool,
    use_128x128_superblock: bool,
    num_planes: u8,
    subsampling: (u8, u8),
) -> IResult<BitInput, (), VerboseError<BitInput>> {
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
            let (input, _lr_unit_shift) = take_bool_bit(input)?;
            input
        } else {
            let (input, lr_unit_shift) = take_bool_bit(input)?;
            if lr_unit_shift {
                let (input, _lr_unit_extra_shift) = take_bool_bit(input)?;
                input
            } else {
                input
            }
        };
        if subsampling.0 > 0 && subsampling.1 > 0 && uses_chroma_lr {
            let (input, _lr_uv_shift) = take_bool_bit(input)?;
            input
        } else {
            input
        }
    } else {
        input
    };

    Ok((input, ()))
}

fn read_tx_mode(
    input: BitInput,
    coded_lossless: bool,
) -> IResult<BitInput, (), VerboseError<BitInput>> {
    let input = if coded_lossless {
        input
    } else {
        let (input, _tx_mode_select) = take_bool_bit(input)?;
        input
    };
    Ok((input, ()))
}

fn frame_reference_mode(
    input: BitInput,
    frame_is_intra: bool,
) -> IResult<BitInput, bool, VerboseError<BitInput>> {
    Ok(if frame_is_intra {
        (input, false)
    } else {
        take_bool_bit(input)?
    })
}

fn skip_mode_params<'a, 'b>(
    input: BitInput<'a>,
    frame_is_intra: bool,
    reference_select: bool,
    order_hint_bits: usize,
    order_hint: u64,
    ref_order_hint: &'b [u64],
    ref_frame_idx: &'b [usize],
) -> IResult<BitInput<'a>, (), VerboseError<BitInput<'a>>> {
    let skip_mode_allowed;
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

    let (input, _skip_mode_present) = if skip_mode_allowed {
        take_bool_bit(input)?
    } else {
        (input, false)
    };

    Ok((input, ()))
}

const fn get_relative_dist(a: i64, b: i64, order_hint_bits: usize) -> i64 {
    if order_hint_bits == 0 {
        return 0;
    }

    let diff = a - b;
    let m = 1 << (order_hint_bits - 1);
    (diff & (m - 1)) - (diff & m)
}

fn global_motion_params(
    input: BitInput,
    frame_is_intra: bool,
) -> IResult<BitInput, (), VerboseError<BitInput>> {
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
                let (input, _is_translation) = take_bool_bit(input)?;
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
#[allow(dead_code)]
pub enum RefType {
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
    feature_data: Option<&SegmentationData>,
) -> u8 {
    if seg_feature_active_idx(segment_id, SEG_LVL_ALT_Q, feature_data) {
        let data = feature_data.unwrap()[segment_id][SEG_LVL_ALT_Q].unwrap();
        let mut qindex = i16::from(base_q_idx) + data;
        if !ignore_delta_q {
            if let Some(current_q_index) = current_q_index {
                qindex = i16::from(current_q_index) + data;
            }
        }
        return clamp(qindex, 0, 255) as u8;
    } else if !ignore_delta_q && current_q_index.is_some() {
        if let Some(current_q_index) = current_q_index {
            return current_q_index;
        }
    }
    base_q_idx
}

#[inline(always)]
fn seg_feature_active_idx(
    segment_id: usize,
    feature: usize,
    feature_data: Option<&SegmentationData>,
) -> bool {
    feature_data.is_some() && feature_data.unwrap()[segment_id][feature].is_some()
}
