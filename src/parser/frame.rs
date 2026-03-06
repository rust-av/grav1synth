use std::cmp::{max, min};

use av1_grain::DEFAULT_GRAIN_SEED;
use bit::BitIndex;
use bitvec::{order::Msb0, view::BitView};
use log::{debug, trace};
use nom::{
    IResult, Parser,
    bits::{bits, complete as bit_parsers},
    error::{Error, context},
};
use num_enum::TryFromPrimitive;
use num_traits::{PrimInt, clamp};

use super::{
    BitstreamParser,
    grain::{FilmGrainHeader, film_grain_params},
    obu::ObuHeader,
    sequence::{SELECT_INTEGER_MV, SELECT_SCREEN_CONTENT_TOOLS},
    trace::{
        TraceCtx, trace_bool, trace_byte_alignment, trace_field, trace_field_signed, trace_su,
        trace_take_u8, trace_take_u32, trace_take_u64, trace_take_usize,
    },
    util::{BitInput, ns, su, take_bool_bit},
};
use crate::{GrainTableSegment, misc::to_binary_string};

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
    /// Parses a full frame OBU payload.
    ///
    /// This reads the frame header first, then parses the tile group section
    /// with the remaining byte budget computed from the OBU payload size.
    /// Returns the parsed [`FrameHeader`] only when the frame is shown.
    pub fn parse_frame_obu<'a>(
        &mut self,
        input: &'a [u8],
        obu_header: ObuHeader,
        // Once again, this is in 10,000,000ths of a second
        packet_ts: u64,
        obu_bit_offset: usize,
    ) -> IResult<&'a [u8], Option<FrameHeader>, Error<&'a [u8]>> {
        let input_len = input.len();
        let (input, frame_header) = context("Failed parsing frame header", |input| {
            self.parse_frame_header(input, obu_header, packet_ts, obu_bit_offset, true)
        })
        .parse(input)?;
        let ref_frame_header = frame_header
            .clone()
            .or_else(|| self.previous_frame_header.clone())
            .unwrap();
        // A reminder that obu size is in bytes
        let tile_group_obu_size = self.size - (input_len - input.len());
        let (input, _) = context("Failed parsing tile group obu", |input| {
            self.parse_tile_group_obu(input, tile_group_obu_size, ref_frame_header.tile_info, 0)
        })
        .parse(input)?;
        Ok((input, frame_header))
    }

    /// Parses the AV1 frame header section needed by grain processing.
    ///
    /// Returns `None` when the parser has already consumed a frame header for
    /// the current temporal unit, or when the frame is not displayed.
    ///
    /// RATIONALE: film grain parameters live at the tail of the uncompressed
    /// header, so this parser must walk the entire header bitstream to reach
    /// them even when most fields are not otherwise used.
    pub fn parse_frame_header<'a>(
        &mut self,
        input: &'a [u8],
        obu_header: ObuHeader,
        // Once again, this is in 10,000,000ths of a second
        packet_ts: u64,
        obu_bit_offset: usize,
        verify_byte_alignment: bool,
    ) -> IResult<&'a [u8], Option<FrameHeader>, Error<&'a [u8]>> {
        if self.seen_frame_header {
            debug!("Seen frame header, exiting frame header parsing");
            return Ok((input, None));
        }

        self.seen_frame_header = true;

        let pre_len = input.len();
        let (input, header) = self.uncompressed_header(
            input,
            obu_header,
            packet_ts,
            obu_bit_offset,
            verify_byte_alignment,
        )?;
        debug!(
            "Consumed {} bytes in uncompressed header",
            pre_len - input.len()
        );
        if header.show_existing_frame {
            let pre_len = input.len();
            let (input, _) = decode_frame_wrapup(input)?;
            debug!(
                "Consumed {} bytes in decode_frame_wrapup",
                pre_len - input.len()
            );
            self.seen_frame_header = false;
            Ok((input, header.show_frame.then_some(header)))
        } else {
            self.seen_frame_header = true;
            Ok((input, header.show_frame.then_some(header)))
        }
    }

    /// Parses `uncompressed_header()` fields and materializes frame state.
    ///
    /// In write mode, this also rewrites film-grain syntax bits based on the
    /// selected [`GrainTableSegment`] that matches `packet_ts`.
    #[allow(clippy::cognitive_complexity)]
    #[allow(clippy::too_many_lines)]
    fn uncompressed_header<'a>(
        &mut self,
        input: &'a [u8],
        obu_headers: ObuHeader,
        // Once again, this is in 10,000,000ths of a second
        packet_ts: u64,
        obu_bit_offset: usize,
        verify_byte_alignment: bool,
    ) -> IResult<&'a [u8], FrameHeader, Error<&'a [u8]>> {
        let orig_input = input;

        bits(|input| {
            let ctx = TraceCtx::new(input, obu_bit_offset);
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
                (input, FrameType::Key, true, true, false, false)
            } else {
                let (input, show_existing_frame) = trace_bool(input, ctx, "show_existing_frame")?;
                if show_existing_frame {
                    let (input, _frame_to_show_map_idx) =
                        trace_take_u8(input, ctx, 3, "frame_to_show_map_idx")?;
                    let input = if let Some(id_len) = id_len {
                        let (input, _display_frame_id) =
                            trace_take_u64(input, ctx, id_len, "display_frame_id")?;
                        input
                    } else {
                        input
                    };

                    if WRITE {
                        let len = orig_input.len() - input.0.len() + usize::from(input.1 > 0);
                        self.packet_out.extend_from_slice(&orig_input[..len]);
                        debug!("Uncompressed header extended by {} bytes", len);
                    }
                    let input = if verify_byte_alignment {
                        trace_byte_alignment(input, ctx)?.0
                    } else {
                        input
                    };
                    return Ok((
                        input,
                        FrameHeader {
                            show_frame: true,
                            show_existing_frame,
                            film_grain_params: FilmGrainHeader::CopyRefFrame,
                            tile_info: self.previous_frame_header.as_ref().unwrap().tile_info,
                        },
                    ));
                }
                let (input, frame_type) = trace_take_u8(input, ctx, 2, "frame_type")?;
                let frame_type = FrameType::try_from(frame_type).unwrap();
                let (input, show_frame) = trace_bool(input, ctx, "show_frame")?;
                let input = if show_frame
                    && let Some(decoder_model_info) = sequence_header.decoder_model_info
                    && !sequence_header
                        .timing_info
                        .is_some_and(|ti| ti.equal_picture_interval)
                {
                    temporal_point_info(
                        input,
                        ctx,
                        decoder_model_info.frame_presentation_time_length_minus_1 as usize + 1,
                    )?
                    .0
                } else {
                    input
                };
                let (input, showable_frame) = if show_frame {
                    (input, frame_type != FrameType::Key)
                } else {
                    trace_bool(input, ctx, "showable_frame")?
                };
                let (input, error_resilient_mode) = if frame_type == FrameType::Switch
                    || (frame_type == FrameType::Key && show_frame)
                {
                    (input, true)
                } else {
                    trace_bool(input, ctx, "error_resilient_mode")?
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

            let (input, disable_cdf_update) = trace_bool(input, ctx, "disable_cdf_update")?;
            let (input, allow_screen_content_tools) =
                if sequence_header.force_screen_content_tools == SELECT_SCREEN_CONTENT_TOOLS {
                    trace_bool(input, ctx, "allow_screen_content_tools")?
                } else {
                    (input, sequence_header.force_screen_content_tools == 1)
                };
            let input = if allow_screen_content_tools
                && sequence_header.force_integer_mv == SELECT_INTEGER_MV
            {
                trace_bool(input, ctx, "force_integer_mv")?.0
            } else {
                input
            };
            let input = if sequence_header.frame_id_numbers_present {
                let (input, _current_frame_id) =
                    trace_take_usize(input, ctx, id_len.unwrap(), "current_frame_id")?;
                input
            } else {
                input
            };
            let (input, frame_size_override_flag) = if frame_type == FrameType::Switch {
                (input, true)
            } else if sequence_header.reduced_still_picture_header {
                (input, false)
            } else {
                trace_bool(input, ctx, "frame_size_override_flag")?
            };
            let (input, order_hint) =
                trace_take_u64(input, ctx, sequence_header.order_hint_bits, "order_hint")?;
            let (input, primary_ref_frame) = if frame_type.is_intra() || error_resilient_mode {
                (input, PRIMARY_REF_NONE)
            } else {
                trace_take_u8(input, ctx, 3, "primary_ref_frame")?
            };

            let mut input = input;
            if let Some(decoder_model_info) = sequence_header.decoder_model_info {
                let (inner_input, buffer_removal_time_present_flag) =
                    trace_bool(input, ctx, "buffer_removal_time_present_flag")?;
                if buffer_removal_time_present_flag {
                    for op_num in 0..=sequence_header.operating_points_cnt_minus_1 {
                        if sequence_header.decoder_model_present_for_op[op_num] {
                            let op_pt_idc = sequence_header.operating_point_idc[op_num];
                            let temporal_id =
                                obu_headers.extension.map_or(0, |ext| ext.temporal_id);
                            let spatial_id = obu_headers.extension.map_or(0, |ext| ext.spatial_id);
                            let in_temporal_layer = (op_pt_idc >> temporal_id) & 1 > 0;
                            let in_spatial_layer = (op_pt_idc >> (spatial_id + 8)) & 1 > 0;
                            if op_pt_idc == 0 || (in_temporal_layer && in_spatial_layer) {
                                let n = decoder_model_info.buffer_removal_time_length_minus_1 + 1;
                                let (inner_input, _buffer_removal_time) = trace_take_u64(
                                    inner_input,
                                    ctx,
                                    usize::from(n),
                                    &format!("buffer_removal_time[{op_num}]"),
                                )?;
                                input = inner_input;
                            }
                        }
                    }
                }
            }

            let mut allow_intrabc = false;
            let (input, refresh_frame_flags) = if frame_type == FrameType::Switch
                || (frame_type == FrameType::Key && show_frame)
            {
                (input, REFRESH_ALL_FRAMES)
            } else {
                trace_take_u8(input, ctx, 8, "refresh_frame_flags")?
            };

            let mut input = input;
            if (!frame_type.is_intra() || refresh_frame_flags != REFRESH_ALL_FRAMES)
                && error_resilient_mode
                && sequence_header.enable_order_hint()
            {
                for i in 0..NUM_REF_FRAMES {
                    let (inner_input, cur_ref_order_hint) = trace_take_u64(
                        input,
                        ctx,
                        sequence_header.order_hint_bits,
                        &format!("ref_order_hint[{i}]"),
                    )?;
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

            let mut allow_high_precision_mv = false;

            let (input, use_ref_frame_mvs, frame_size, upscaled_size) = if frame_type.is_intra() {
                let (input, frame_size) = frame_size(
                    input,
                    ctx,
                    frame_size_override_flag,
                    sequence_header.enable_superres,
                    sequence_header.frame_width_bits_minus_1 + 1,
                    sequence_header.frame_height_bits_minus_1 + 1,
                    max_frame_size,
                )?;
                let upscaled_size = frame_size;
                let (input, _render_size) = render_size(input, ctx, frame_size, upscaled_size)?;
                (
                    if allow_screen_content_tools && upscaled_size.width == frame_size.width {
                        let (input, allow_intrabc_inner) = trace_bool(input, ctx, "allow_intrabc")?;
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
                let (mut input, frame_refs_short_signaling) = if sequence_header.enable_order_hint()
                {
                    let (input, frame_refs_short_signaling) =
                        trace_bool(input, ctx, "frame_refs_short_signaling")?;
                    if frame_refs_short_signaling {
                        let (input, _last_frame_idx) =
                            trace_take_u8(input, ctx, 3, "last_frame_idx")?;
                        let (input, _gold_frame_idx) =
                            trace_take_u8(input, ctx, 3, "gold_frame_idx")?;
                        let (input, _) = set_frame_refs(input)?;
                        (input, frame_refs_short_signaling)
                    } else {
                        (input, frame_refs_short_signaling)
                    }
                } else {
                    (input, false)
                };

                for (i, ref_frame_idx) in self.ref_frame_idx.iter_mut().enumerate() {
                    if frame_refs_short_signaling {
                        *ref_frame_idx = 0;
                    } else {
                        let (inner_input, this_ref_frame_idx) =
                            trace_take_usize(input, ctx, 3, &format!("ref_frame_idx[{i}]"))?;
                        input = inner_input;
                        *ref_frame_idx = this_ref_frame_idx;
                        if sequence_header.frame_id_numbers_present {
                            let n = sequence_header.delta_frame_id_len_minus_2 + 2;
                            let (inner_input, _delta_frame_id_minus_1) = trace_take_u64(
                                input,
                                ctx,
                                n,
                                &format!("delta_frame_id_minus_1[{i}]"),
                            )?;
                            input = inner_input;
                        }
                    }
                }
                let (input, frame_size, upscaled_size) = if frame_size_override_flag
                    && !error_resilient_mode
                {
                    let mut frame_size = max_frame_size;
                    let mut upscaled_size = frame_size;
                    let (input, frame_size) = frame_size_with_refs(
                        input,
                        ctx,
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
                        ctx,
                        frame_size_override_flag,
                        sequence_header.enable_superres,
                        sequence_header.frame_width_bits_minus_1 + 1,
                        sequence_header.frame_height_bits_minus_1 + 1,
                        max_frame_size,
                    )?;
                    let upscaled_size = frame_size;
                    let (input, _render_size) = render_size(input, ctx, frame_size, upscaled_size)?;
                    (input, frame_size, upscaled_size)
                };
                let (input, allow_high_precision_mv_new) = if sequence_header.force_integer_mv == 1
                {
                    (input, false)
                } else {
                    trace_bool(input, ctx, "allow_high_precision_mv")?
                };
                allow_high_precision_mv = allow_high_precision_mv_new;

                let (input, _) = read_interpolation_filter(input, ctx)?;
                let (input, _is_motion_mode_switchable) =
                    trace_bool(input, ctx, "is_motion_mode_switchable")?;
                let (input, use_ref_frame_mvs) =
                    if error_resilient_mode || !sequence_header.enable_ref_frame_mvs {
                        (input, false)
                    } else {
                        trace_bool(input, ctx, "use_ref_frame_mvs")?
                    };
                for i in 0..REFS_PER_FRAME {
                    let ref_frame = RefType::Last as usize + i;
                    let hint = self.big_ref_order_hint[self.ref_frame_idx[i]];
                    self.big_order_hints[ref_frame] = hint;
                }
                (input, use_ref_frame_mvs, frame_size, upscaled_size)
            };
            let (mi_cols, mi_rows) = compute_image_size(frame_size);

            let (input, _disable_frame_end_update_cdf) =
                if sequence_header.reduced_still_picture_header || disable_cdf_update {
                    (input, true)
                } else {
                    trace_bool(input, ctx, "disable_frame_end_update_cdf")?
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
                ctx,
                sequence_header.use_128x128_superblock,
                mi_cols,
                mi_rows,
            )?;
            let (input, q_params) = quantization_params(
                input,
                ctx,
                sequence_header.color_config.num_planes,
                sequence_header.color_config.separate_uv_delta_q,
            )?;
            let (input, segmentation_data) = segmentation_params(input, ctx, primary_ref_frame)?;
            let (input, delta_q_present) = delta_q_params(input, ctx, q_params.base_q_idx)?;
            let (input, _) = delta_lf_params(input, ctx, delta_q_present, allow_intrabc)?;
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
                ctx,
                coded_lossless,
                allow_intrabc,
                sequence_header.color_config.num_planes,
            )?;
            let (input, _) = cdef_params(
                input,
                ctx,
                coded_lossless,
                allow_intrabc,
                sequence_header.enable_cdef,
                sequence_header.color_config.num_planes,
            )?;
            let (input, _) = lr_params(
                input,
                ctx,
                all_losslesss,
                allow_intrabc,
                sequence_header.enable_restoration,
                sequence_header.use_128x128_superblock,
                sequence_header.color_config.num_planes,
                sequence_header.color_config.subsampling,
            )?;
            let (input, _) = read_tx_mode(input, ctx, coded_lossless)?;
            let (input, reference_select) =
                frame_reference_mode(input, ctx, frame_type.is_intra())?;
            let (input, _) = skip_mode_params(
                input,
                ctx,
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
                trace_bool(input, ctx, "allow_warped_motion")?
            };
            let (input, _reduced_tx_set) = trace_bool(input, ctx, "reduced_tx_set")?;
            let (input, _) =
                global_motion_params(input, ctx, frame_type.is_intra(), allow_high_precision_mv)?;

            let film_grain_allowed = show_frame || showable_frame;
            let written_film_grain_params = if WRITE {
                let len = orig_input.len() - input.0.len();
                self.packet_out.extend_from_slice(&orig_input[..len]);
                debug!("Pre-film-grain header extended by {} bytes", len);
                if sequence_header.new_film_grain_state && film_grain_allowed {
                    // There will always be at least 1 bit left that we can read
                    let extra_byte = orig_input[len];
                    let extra_bits_used = input.1;
                    if let Some(new_header) = self
                        .incoming_grain_header
                        .as_mut()
                        .and_then(|segments| {
                            let mut segment = segments.iter_mut().find(|seg| {
                                seg.start_time <= packet_ts && seg.end_time >= packet_ts
                            });
                            if let Some(segment) = segment.as_mut() {
                                segment.grain_params.grain_seed = segment
                                    .grain_params
                                    .grain_seed
                                    .wrapping_add(DEFAULT_GRAIN_SEED);
                            }
                            segment
                        })
                        .cloned()
                    {
                        self.write_film_grain_bits(
                            extra_byte,
                            extra_bits_used,
                            &new_header,
                            frame_type,
                        )
                    } else {
                        // Sets "apply_grain" to false. We don't need to do anything else.
                        self.write_film_grain_disabled_bit(extra_byte, extra_bits_used);
                        FilmGrainHeader::Disable
                    }
                } else {
                    // There won't be any bits remaining, and we don't need to append any bits,
                    // so we have to handle this separately.
                    if input.1 > 0 {
                        let mut extra_byte = orig_input[len];
                        let start_bit = 7 - input.1;
                        for i in 0..=start_bit {
                            extra_byte.set_bit(i, false);
                        }
                        self.packet_out.push(extra_byte);
                    }
                    FilmGrainHeader::Disable
                }
            } else {
                FilmGrainHeader::Disable
            };

            let sequence_header = self.sequence_header.as_ref().unwrap();
            let (input, parsed_film_grain_params) = film_grain_params(
                input,
                ctx,
                sequence_header.film_grain_params_present && film_grain_allowed,
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

            let input = if verify_byte_alignment {
                trace_byte_alignment(input, ctx)?.0
            } else {
                input
            };

            Ok((
                input,
                FrameHeader {
                    show_frame,
                    show_existing_frame,
                    film_grain_params: if WRITE {
                        written_film_grain_params
                    } else {
                        parsed_film_grain_params
                    },
                    tile_info,
                },
            ))
        })(input)
    }

    /// Serializes replacement film-grain syntax into the output packet buffer.
    ///
    /// `extra_byte` and `extra_bits_used` preserve already-consumed prefix bits
    /// from the partially-read source byte before writing new grain fields.
    fn write_film_grain_bits(
        &mut self,
        extra_byte: u8,
        extra_bits_used: usize,
        new_header: &GrainTableSegment,
        frame_type: FrameType,
    ) -> FilmGrainHeader {
        let params = &new_header.grain_params;
        let mut data = bitvec::bitvec![u8, Msb0;];

        for i in 0..extra_bits_used {
            data.push(extra_byte.bit(7 - i));
        }

        // Set "apply_grain" to true.
        data.push(true);
        // Grain seed (16 bits)
        data.extend(params.grain_seed.view_bits::<Msb0>());
        // update_grain flag (1 bit)
        if frame_type == FrameType::Inter {
            data.push(true);
        }
        // Y points
        let num_y_points = params.scaling_points_y.len() as u8;
        data.extend(&num_y_points.view_bits::<Msb0>()[4..]);
        for point in &params.scaling_points_y {
            data.extend(point[0].view_bits::<Msb0>());
            data.extend(point[1].view_bits::<Msb0>());
        }
        // Chroma scaling from luma
        let color_config = &self.sequence_header.as_ref().unwrap().color_config;
        let monochrome = color_config.num_planes == 1;
        let chroma_scaling_from_luma = if monochrome {
            false
        } else {
            let scaling = params.chroma_scaling_from_luma;
            data.push(scaling);
            scaling
        };
        // Chroma points
        let (num_cb_points, num_cr_points) = if monochrome
            || chroma_scaling_from_luma
            || (color_config.subsampling == (1, 1) && num_y_points == 0)
        {
            (0, 0)
        } else {
            let cb_points = params.scaling_points_cb.len() as u8;
            data.extend(&cb_points.view_bits::<Msb0>()[4..]);
            for point in &params.scaling_points_cb {
                data.extend(point[0].view_bits::<Msb0>());
                data.extend(point[1].view_bits::<Msb0>());
            }

            let cr_points = params.scaling_points_cr.len() as u8;
            data.extend(&cr_points.view_bits::<Msb0>()[4..]);
            for point in &params.scaling_points_cr {
                data.extend(point[0].view_bits::<Msb0>());
                data.extend(point[1].view_bits::<Msb0>());
            }

            (cb_points, cr_points)
        };
        // Grain scaling minus 8 (2 bits)
        data.extend(&(params.scaling_shift - 8).view_bits::<Msb0>()[6..]);
        // ar_coeff_lag (2 bits)
        data.extend(&(params.ar_coeff_lag).view_bits::<Msb0>()[6..]);
        // ar_coeffs_y
        let num_pos_luma = 2 * params.ar_coeff_lag as usize * (params.ar_coeff_lag as usize + 1);
        let num_pos_chroma = if num_y_points > 0 {
            for point in &params.ar_coeffs_y[..num_pos_luma] {
                let point = (i16::from(*point) + 128) as u8;
                data.extend(point.view_bits::<Msb0>());
            }
            num_pos_luma + 1
        } else {
            num_pos_luma
        };
        // ar_coeffs chroma
        if chroma_scaling_from_luma || num_cb_points > 0 {
            for point in &params.ar_coeffs_cb[..num_pos_chroma] {
                let point = (i16::from(*point) + 128) as u8;
                data.extend(point.view_bits::<Msb0>());
            }
        }
        if chroma_scaling_from_luma || num_cr_points > 0 {
            for point in &params.ar_coeffs_cr[..num_pos_chroma] {
                let point = (i16::from(*point) + 128) as u8;
                data.extend(point.view_bits::<Msb0>());
            }
        }
        // ar coeff shift minus 6 (2 bits)
        data.extend(&(params.ar_coeff_shift - 6).view_bits::<Msb0>()[6..]);
        // grain scale shift (2 bits)
        data.extend(&(params.grain_scale_shift).view_bits::<Msb0>()[6..]);
        // chroma multis
        if num_cb_points > 0 {
            data.extend((params.cb_mult).view_bits::<Msb0>());
            data.extend((params.cb_luma_mult).view_bits::<Msb0>());
            data.extend(&(params.cb_offset).view_bits::<Msb0>()[7..]);
        }
        if num_cr_points > 0 {
            data.extend((params.cr_mult).view_bits::<Msb0>());
            data.extend((params.cr_luma_mult).view_bits::<Msb0>());
            data.extend(&(params.cr_offset).view_bits::<Msb0>()[7..]);
        }
        // overlap flag (1 bit)
        data.push(params.overlap_flag);
        // clip_to_restricted_range flag (1 bit)
        data.push(params.clip_to_restricted_range);

        self.packet_out.extend_from_slice(data.as_raw_slice());
        trace!(
            "Film grain packet contents: {}",
            to_binary_string(data.as_raw_slice())
        );

        FilmGrainHeader::UpdateGrain(params.clone())
    }

    /// Writes an `apply_grain = 0` bit while preserving partial-byte alignment.
    fn write_film_grain_disabled_bit(&mut self, extra_byte: u8, extra_bits_used: usize) {
        let mut data = bitvec::bitvec![u8, Msb0;];

        for i in 0..extra_bits_used {
            data.push(extra_byte.bit(7 - i));
        }
        // Set "apply_grain" to false.
        data.push(false);

        self.packet_out.extend_from_slice(data.as_raw_slice());
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
    /// Returns `true` for intra-coded frame types.
    pub fn is_intra(self) -> bool {
        self == FrameType::Key || self == FrameType::IntraOnly
    }
}

/// Placeholder for AV1 `decode_frame_wrapup()`.
///
/// RATIONALE: this parser only needs header traversal and does not model the
/// decoder-side wrap-up operations.
#[allow(clippy::unnecessary_wraps)]
const fn decode_frame_wrapup(input: &[u8]) -> IResult<&[u8], (), Error<&[u8]>> {
    Ok((input, ()))
}

/// Reads and discards temporal-point timing metadata when present.
fn temporal_point_info<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    frame_presentation_time_length: usize,
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
    let (input, _frame_presentation_time) = trace_take_u64(
        input,
        ctx,
        frame_presentation_time_length,
        "frame_presentation_time",
    )?;
    Ok((input, ()))
}

#[derive(Debug, Clone, Copy)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

/// Parses coded frame dimensions and applies super-resolution scaling.
fn frame_size<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    frame_size_override: bool,
    enable_superres: bool,
    frame_width_bits: usize,
    frame_height_bits: usize,
    max_frame_size: Dimensions,
) -> IResult<BitInput<'a>, Dimensions, Error<BitInput<'a>>> {
    let (input, width, height) = if frame_size_override {
        let (input, width_minus_1) =
            trace_take_u32(input, ctx, frame_width_bits, "frame_width_minus_1")?;
        let (input, height_minus_1) =
            trace_take_u32(input, ctx, frame_height_bits, "frame_height_minus_1")?;
        (input, width_minus_1 + 1, height_minus_1 + 1)
    } else {
        (input, max_frame_size.width, max_frame_size.height)
    };
    let mut frame_size = Dimensions { width, height };
    let mut upscaled_size = frame_size;
    let (input, _) = superres_params(
        input,
        ctx,
        enable_superres,
        &mut frame_size,
        &mut upscaled_size,
    )?;
    Ok((input, frame_size))
}

/// Parses optional render dimensions for display sizing.
fn render_size<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    frame_size: Dimensions,
    upscaled_size: Dimensions,
) -> IResult<BitInput<'a>, Dimensions, Error<BitInput<'a>>> {
    let (input, render_and_frame_size_different) =
        trace_bool(input, ctx, "render_and_frame_size_different")?;
    let (input, width, height) = if render_and_frame_size_different {
        let (input, render_width_minus_1) = trace_take_u32(input, ctx, 16, "render_width_minus_1")?;
        let (input, render_height_minus_1) =
            trace_take_u32(input, ctx, 16, "render_height_minus_1")?;
        (input, render_width_minus_1 + 1, render_height_minus_1 + 1)
    } else {
        (input, upscaled_size.width, frame_size.height)
    };
    Ok((input, Dimensions { width, height }))
}

/// Placeholder for AV1 `set_frame_refs()`.
///
/// RATIONALE: reference remapping side effects are not required for the
/// grain/header operations implemented by this tool.
#[allow(clippy::unnecessary_wraps)]
const fn set_frame_refs(input: BitInput) -> IResult<BitInput, (), Error<BitInput>> {
    Ok((input, ()))
}

/// Parses frame size using reference-frame signaling when available.
///
/// If no reference size is selected, falls back to explicit frame-size syntax.
#[allow(clippy::too_many_arguments)]
fn frame_size_with_refs<'a, 'b>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    enable_superres: bool,
    frame_size_override: bool,
    frame_width_bits: usize,
    frame_height_bits: usize,
    max_frame_size: Dimensions,
    ref_frame_size: &'b mut Dimensions,
    ref_upscaled_size: &'b mut Dimensions,
) -> IResult<BitInput<'a>, Dimensions, Error<BitInput<'a>>> {
    let mut found_ref = false;
    let mut input = input;
    for i in 0..REFS_PER_FRAME {
        let (inner_input, found_this_ref) = trace_bool(input, ctx, &format!("found_ref[{i}]"))?;
        input = inner_input;
        if found_this_ref {
            found_ref = true;
            break;
        }
    }
    let (input, frame_size) = if found_ref {
        let (input, _) = superres_params(
            input,
            ctx,
            enable_superres,
            ref_frame_size,
            ref_upscaled_size,
        )?;
        (input, *ref_frame_size)
    } else {
        let (input, frame_size) = frame_size(
            input,
            ctx,
            frame_size_override,
            enable_superres,
            frame_width_bits,
            frame_height_bits,
            max_frame_size,
        )?;
        let (input, _) = render_size(input, ctx, frame_size, *ref_upscaled_size)?;
        (input, frame_size)
    };
    Ok((input, frame_size))
}

/// Parses super-resolution parameters and updates frame/upscaled dimensions.
fn superres_params<'a, 'b>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    enable_superres: bool,
    frame_size: &'b mut Dimensions,
    upscaled_size: &'b mut Dimensions,
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
    let (input, use_superres) = if enable_superres {
        trace_bool(input, ctx, "use_superres")?
    } else {
        (input, false)
    };
    let (input, superres_denom) = if use_superres {
        let (input, coded_denom) = trace_take_u32(input, ctx, SUPERRES_DENOM_BITS, "coded_denom")?;
        (input, coded_denom + SUPERRES_DENOM_MIN)
    } else {
        (input, SUPERRES_NUM)
    };
    upscaled_size.width = frame_size.width;
    frame_size.width = (upscaled_size.width * SUPERRES_NUM + (superres_denom / 2)) / superres_denom;
    Ok((input, ()))
}

/// Converts frame dimensions into MI (mode info) grid dimensions.
#[must_use]
const fn compute_image_size(frame_size: Dimensions) -> (u32, u32) {
    let mi_cols = 2 * ((frame_size.width + 7) >> 3u8);
    let mi_rows = 2 * ((frame_size.height + 7) >> 3u8);
    (mi_cols, mi_rows)
}

/// Parses interpolation-filter selection syntax.
fn read_interpolation_filter<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
    let (input, is_filter_switchable) = trace_bool(input, ctx, "is_filter_switchable")?;
    let (input, _interpolation_filter) = if is_filter_switchable {
        (input, INTERP_FILTER_SWITCHABLE)
    } else {
        trace_take_u8(input, ctx, 2, "interpolation_filter")?
    };
    Ok((input, ()))
}

/// Placeholder for CDF initialization when no primary reference is used.
#[allow(clippy::unnecessary_wraps)]
const fn init_non_coeff_cdfs(input: BitInput) -> IResult<BitInput, (), Error<BitInput>> {
    Ok((input, ()))
}

/// Placeholder for AV1 `setup_past_independence()` state reset.
#[allow(clippy::unnecessary_wraps)]
const fn setup_past_independence(input: BitInput) -> IResult<BitInput, (), Error<BitInput>> {
    Ok((input, ()))
}

/// Placeholder for loading probability models from a reference frame.
#[allow(clippy::unnecessary_wraps)]
const fn load_cdfs(input: BitInput) -> IResult<BitInput, (), Error<BitInput>> {
    Ok((input, ()))
}

/// Placeholder for loading decoder state from a primary reference frame.
#[allow(clippy::unnecessary_wraps)]
const fn load_previous(input: BitInput) -> IResult<BitInput, (), Error<BitInput>> {
    Ok((input, ()))
}

/// Placeholder for AV1 motion-field estimation side effects.
#[allow(clippy::unnecessary_wraps)]
const fn motion_field_estimation(input: BitInput) -> IResult<BitInput, (), Error<BitInput>> {
    Ok((input, ()))
}

#[allow(clippy::too_many_lines)]
/// Parses tile layout syntax and returns derived tile-grid metadata.
///
/// The returned [`TileInfo`] is reused by tile-group parsing to determine how
/// many tile units are expected in this frame.
fn tile_info<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    use_128x128_superblock: bool,
    mi_cols: u32,
    mi_rows: u32,
) -> IResult<BitInput<'a>, TileInfo, Error<BitInput<'a>>> {
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
    let tile_rows;
    let tile_cols;

    let (mut input, uniform_tile_spacing_flag) =
        trace_bool(input, ctx, "uniform_tile_spacing_flag")?;
    let (tile_cols_log2, tile_rows_log2) = if uniform_tile_spacing_flag {
        let mut tile_cols_log2 = min_log2_tile_cols;
        while tile_cols_log2 < max_log2_tile_cols {
            let (inner_input, increment_tile_cols_log2) =
                trace_bool(input, ctx, "increment_tile_cols_log2")?;
            input = inner_input;
            if increment_tile_cols_log2 {
                tile_cols_log2 += 1;
            } else {
                break;
            }
        }
        let tile_width_sb = (sb_cols + (1 << tile_cols_log2) - 1) >> tile_cols_log2;
        tile_cols = sb_cols / tile_width_sb;

        let min_log2_tile_rows = max(min_log2_tiles as i32 - tile_cols_log2 as i32, 0i32) as u32;
        let mut tile_rows_log2 = min_log2_tile_rows;
        while tile_rows_log2 < max_log2_tile_rows {
            let (inner_input, increment_tile_rows_log2) =
                trace_bool(input, ctx, "increment_tile_rows_log2")?;
            input = inner_input;
            if increment_tile_rows_log2 {
                tile_rows_log2 += 1;
            } else {
                break;
            }
        }
        let tile_height_sb = (sb_rows + (1 << tile_rows_log2) - 1) >> tile_rows_log2;
        tile_rows = sb_rows / tile_height_sb;

        (tile_cols_log2, tile_rows_log2)
    } else {
        let mut widest_tile_sb = 0;
        let mut start_sb = 0;
        let mut i = 0;
        while start_sb < sb_cols {
            let max_width = min(sb_cols - start_sb, max_tile_width_sb);
            let pos = ctx.pos(input);
            let (inner_input, width_in_sbs_minus_1) = ns(input, max_width as usize)?;
            let bits_consumed = ctx.pos(inner_input) - pos;
            trace_field(
                pos,
                &format!("width_in_sbs_minus_1[{i}]"),
                bits_consumed,
                width_in_sbs_minus_1,
            );
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
            let pos = ctx.pos(input);
            let (inner_input, height_in_sbs_minus_1) = ns(input, max_height as usize)?;
            let bits_consumed = ctx.pos(inner_input) - pos;
            trace_field(
                pos,
                &format!("height_in_sbs_minus_1[{i}]"),
                bits_consumed,
                height_in_sbs_minus_1,
            );
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
        let (input, _context_update_tile_id) = trace_take_u64(
            input,
            ctx,
            (tile_rows_log2 + tile_cols_log2) as usize,
            "context_update_tile_id",
        )?;
        let (input, _tile_size_bytes_minus_1) =
            trace_take_u8(input, ctx, 2, "tile_size_bytes_minus_1")?;
        input
    } else {
        input
    };

    Ok((
        input,
        TileInfo {
            tile_cols,
            tile_rows,
            tile_cols_log2,
            tile_rows_log2,
        },
    ))
}

#[derive(Debug, Clone, Copy)]
pub struct TileInfo {
    pub tile_cols: u32,
    pub tile_rows: u32,
    pub tile_cols_log2: u32,
    pub tile_rows_log2: u32,
}

/// Returns the smallest `k` where `blk_size << k >= target`.
///
/// RATIONALE: AV1 tile derivation is specified as an iterative loop, so this
/// implementation mirrors the spec wording for readability and auditability.
#[must_use]
fn tile_log2<T: PrimInt>(blk_size: T, target: T) -> T {
    let mut k = 0;
    while (blk_size << k) < target {
        k += 1;
    }
    T::from(k).unwrap()
}

/// Parses frame-level quantization parameters.
fn quantization_params<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    num_planes: u8,
    separate_uv_delta_q: bool,
) -> IResult<BitInput<'a>, QuantizationParams, Error<BitInput<'a>>> {
    let (input, base_q_idx) = trace_take_u8(input, ctx, 8, "base_q_idx")?;
    let (input, deltaq_y_dc) = read_delta_q(input, ctx, "delta_q_y_dc")?;
    let (input, deltaq_u_dc, deltaq_u_ac, deltaq_v_dc, deltaq_v_ac) = if num_planes > 1 {
        let (input, diff_uv_delta) = if separate_uv_delta_q {
            trace_bool(input, ctx, "diff_uv_delta")?
        } else {
            (input, false)
        };
        let (input, deltaq_u_dc) = read_delta_q(input, ctx, "delta_q_u_dc")?;
        let (input, deltaq_u_ac) = read_delta_q(input, ctx, "delta_q_u_ac")?;
        let (input, deltaq_v_dc, deltaq_v_ac) = if diff_uv_delta {
            let (input, deltaq_v_dc) = read_delta_q(input, ctx, "delta_q_v_dc")?;
            let (input, deltaq_v_ac) = read_delta_q(input, ctx, "delta_q_v_ac")?;
            (input, deltaq_v_dc, deltaq_v_ac)
        } else {
            (input, deltaq_u_dc, deltaq_u_ac)
        };
        (input, deltaq_u_dc, deltaq_u_ac, deltaq_v_dc, deltaq_v_ac)
    } else {
        (input, 0, 0, 0, 0)
    };
    let (input, using_qmatrix) = trace_bool(input, ctx, "using_qmatrix")?;
    let input = if using_qmatrix {
        let (input, _qm_y) = trace_take_u8(input, ctx, 4, "qm_y")?;
        let (input, qm_u) = trace_take_u8(input, ctx, 4, "qm_u")?;
        let (input, _qm_v) = if separate_uv_delta_q {
            trace_take_u8(input, ctx, 4, "qm_v")?
        } else {
            (input, qm_u)
        };

        input
    } else {
        input
    };

    Ok((
        input,
        QuantizationParams {
            base_q_idx,
            deltaq_y_dc,
            deltaq_u_ac,
            deltaq_u_dc,
            deltaq_v_ac,
            deltaq_v_dc,
        },
    ))
}

/// Parses an optionally coded signed quantizer delta.
fn read_delta_q<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
    name: &str,
) -> IResult<BitInput<'a>, i64, Error<BitInput<'a>>> {
    let pos = ctx.pos(input);
    let (input, delta_coded) = take_bool_bit(input)?;
    if delta_coded {
        let (input, value) = su(input, 1 + 6)?;
        let bits_consumed = ctx.pos(input) - pos;
        let raw = (value as u64) & ((1u64 << bits_consumed) - 1);
        trace_field_signed(pos, name, bits_consumed, raw, value);
        Ok((input, value))
    } else {
        trace_field(pos, name, 1, 0);
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

/// Parses segmentation flags and per-segment feature payloads.
///
/// Returns `None` when segmentation is disabled for the frame.
fn segmentation_params<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    primary_ref_frame: u8,
) -> IResult<BitInput<'a>, Option<SegmentationData>, Error<BitInput<'a>>> {
    let mut segmentation_data: SegmentationData = Default::default();
    let (input, segmentation_enabled) = trace_bool(input, ctx, "segmentation_enabled")?;
    let input = if segmentation_enabled {
        let (input, segmentation_update_data) = if primary_ref_frame == PRIMARY_REF_NONE {
            (input, true)
        } else {
            let (input, segmentation_update_map) =
                trace_bool(input, ctx, "segmentation_update_map")?;
            let input = if segmentation_update_map {
                let (input, _segmentation_temporal_update) =
                    trace_bool(input, ctx, "segmentation_temporal_update")?;
                input
            } else {
                input
            };
            trace_bool(input, ctx, "segmentation_update_data")?
        };
        if segmentation_update_data {
            let mut input = input;
            #[allow(clippy::needless_range_loop)]
            for i in 0..MAX_SEGMENTS {
                for j in 0..SEG_LVL_MAX {
                    let (inner_input, feature_enabled) =
                        trace_bool(input, ctx, &format!("feature_enabled[{i}][{j}]"))?;
                    input = if feature_enabled {
                        let bits_to_read = SEGMENTATION_FEATURE_BITS[j] as usize;
                        let limit = i16::from(SEGMENTATION_FEATURE_MAX[j]);
                        let (inner_input, feature_value) = if SEGMENTATION_FEATURE_SIGNED[j] {
                            let (input, value) = trace_su(
                                inner_input,
                                ctx,
                                1 + bits_to_read,
                                &format!("feature_value[{i}][{j}]"),
                            )?;
                            (input, clamp(value as i16, -limit, limit))
                        } else {
                            let (input, value): (_, i16) = trace_take_u32(
                                inner_input,
                                ctx,
                                bits_to_read,
                                &format!("feature_value[{i}][{j}]"),
                            )
                            .map(|(i, v)| (i, v as i16))?;
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
    Ok((input, segmentation_enabled.then_some(segmentation_data)))
}

/// Parses delta-quantization enablement and resolution.
///
/// Returns whether `delta_q` is present for subsequent syntax sections.
fn delta_q_params<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    base_q_idx: u8,
) -> IResult<BitInput<'a>, bool, Error<BitInput<'a>>> {
    let (input, delta_q_present) = if base_q_idx > 0 {
        trace_bool(input, ctx, "delta_q_present")?
    } else {
        (input, false)
    };
    let (input, _delta_q_res) = if delta_q_present {
        trace_take_u8(input, ctx, 2, "delta_q_res")?
    } else {
        (input, 0)
    };
    Ok((input, delta_q_present))
}

/// Parses delta loop-filter parameters when enabled.
fn delta_lf_params<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    delta_q_present: bool,
    allow_intrabc: bool,
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
    let input = if delta_q_present {
        let (input, delta_lf_present) = if allow_intrabc {
            (input, false)
        } else {
            trace_bool(input, ctx, "delta_lf_present")?
        };
        if delta_lf_present {
            let (input, _delta_lf_res) = trace_take_u8(input, ctx, 2, "delta_lf_res")?;
            let (input, _delta_lf_multi) = trace_bool(input, ctx, "delta_lf_multi")?;
            input
        } else {
            input
        }
    } else {
        input
    };
    Ok((input, ()))
}

/// Placeholder for coefficient CDF initialization.
#[allow(clippy::unnecessary_wraps)]
const fn init_coeff_cdfs(input: BitInput) -> IResult<BitInput, (), Error<BitInput>> {
    Ok((input, ()))
}

/// Placeholder for loading previous segment-id maps.
#[allow(clippy::unnecessary_wraps)]
const fn load_previous_segment_ids(input: BitInput) -> IResult<BitInput, (), Error<BitInput>> {
    Ok((input, ()))
}

/// Parses loop-filter syntax for non-lossless inter/intra frames.
fn loop_filter_params<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    coded_lossless: bool,
    allow_intrabc: bool,
    num_planes: u8,
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
    if coded_lossless || allow_intrabc {
        return Ok((input, ()));
    }

    let (input, loop_filter_l0) = trace_take_u8(input, ctx, 6, "loop_filter_level[0]")?;
    let (input, loop_filter_l1) = trace_take_u8(input, ctx, 6, "loop_filter_level[1]")?;
    let input = if num_planes > 1 && (loop_filter_l0 > 0 || loop_filter_l1 > 0) {
        let (input, _loop_filter_l2) = trace_take_u8(input, ctx, 6, "loop_filter_level[2]")?;
        let (input, _loop_filter_l3) = trace_take_u8(input, ctx, 6, "loop_filter_level[3]")?;
        input
    } else {
        input
    };
    let (input, _loop_filter_sharpness) = trace_take_u8(input, ctx, 3, "loop_filter_sharpness")?;
    let (mut input, loop_filter_delta_enabled) =
        trace_bool(input, ctx, "loop_filter_delta_enabled")?;
    if loop_filter_delta_enabled {
        let (inner_input, loop_filter_delta_update) =
            trace_bool(input, ctx, "loop_filter_delta_update")?;
        input = inner_input;
        if loop_filter_delta_update {
            for i in 0..TOTAL_REFS_PER_FRAME {
                let (inner_input, update_ref_delta) =
                    trace_bool(input, ctx, &format!("update_ref_delta[{i}]"))?;
                input = if update_ref_delta {
                    let (inner_input, _loop_filter_ref_delta) = trace_su(
                        inner_input,
                        ctx,
                        1 + 6,
                        &format!("loop_filter_ref_deltas[{i}]"),
                    )?;
                    inner_input
                } else {
                    inner_input
                };
            }
            for i in 0..2u8 {
                let (inner_input, update_mode_delta) =
                    trace_bool(input, ctx, &format!("update_mode_delta[{i}]"))?;
                input = if update_mode_delta {
                    let (inner_input, _loop_filter_mode_delta) = trace_su(
                        inner_input,
                        ctx,
                        1 + 6,
                        &format!("loop_filter_mode_deltas[{i}]"),
                    )?;
                    inner_input
                } else {
                    inner_input
                };
            }
        }
    }

    Ok((input, ()))
}

/// Parses CDEF (constrained directional enhancement filter) parameters.
fn cdef_params<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    coded_lossless: bool,
    allow_intrabc: bool,
    enable_cdef: bool,
    num_planes: u8,
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
    if coded_lossless || allow_intrabc || !enable_cdef {
        return Ok((input, ()));
    }

    let (input, _cdef_damping_minus_3) = trace_take_u8(input, ctx, 2, "cdef_damping_minus_3")?;
    let (mut input, cdef_bits) = trace_take_u8(input, ctx, 2, "cdef_bits")?;
    for i in 0..(1usize << cdef_bits) {
        let (inner_input, _cdef_y_pri_str) =
            trace_take_u8(input, ctx, 4, &format!("cdef_y_pri_strength[{i}]"))?;
        let (inner_input, _cdef_y_sec_str) =
            trace_take_u8(inner_input, ctx, 2, &format!("cdef_y_sec_strength[{i}]"))?;
        input = if num_planes > 1 {
            let (inner_input, _cdef_uv_pri_str) =
                trace_take_u8(inner_input, ctx, 4, &format!("cdef_uv_pri_strength[{i}]"))?;
            let (inner_input, _cdef_uv_sec_str) =
                trace_take_u8(inner_input, ctx, 2, &format!("cdef_uv_sec_strength[{i}]"))?;
            inner_input
        } else {
            inner_input
        }
    }

    Ok((input, ()))
}

/// Parses loop-restoration parameters for all active planes.
#[allow(clippy::too_many_arguments)]
fn lr_params<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    all_lossless: bool,
    allow_intrabc: bool,
    enable_restoration: bool,
    use_128x128_superblock: bool,
    num_planes: u8,
    subsampling: (u8, u8),
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
    if all_lossless || allow_intrabc || !enable_restoration {
        return Ok((input, ()));
    }

    let mut input = input;
    let mut uses_lr = false;
    let mut uses_chroma_lr = false;
    for i in 0..num_planes {
        let (inner_input, lr_type) = trace_take_u8(input, ctx, 2, &format!("lr_type[{i}]"))?;
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
            let (input, _lr_unit_shift) = trace_bool(input, ctx, "lr_unit_shift")?;
            input
        } else {
            let (input, lr_unit_shift) = trace_bool(input, ctx, "lr_unit_shift")?;
            if lr_unit_shift {
                let (input, _lr_unit_extra_shift) = trace_bool(input, ctx, "lr_unit_extra_shift")?;
                input
            } else {
                input
            }
        };
        if subsampling.0 > 0 && subsampling.1 > 0 && uses_chroma_lr {
            let (input, _lr_uv_shift) = trace_bool(input, ctx, "lr_uv_shift")?;
            input
        } else {
            input
        }
    } else {
        input
    };

    Ok((input, ()))
}

/// Parses transform-mode signaling.
fn read_tx_mode<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    coded_lossless: bool,
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
    let input = if coded_lossless {
        input
    } else {
        let (input, _tx_mode_select) = trace_bool(input, ctx, "tx_mode_select")?;
        input
    };
    Ok((input, ()))
}

/// Parses reference-mode signaling.
///
/// Intra frames always return `false` because reference selection is inapplicable.
fn frame_reference_mode<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    frame_is_intra: bool,
) -> IResult<BitInput<'a>, bool, Error<BitInput<'a>>> {
    Ok(if frame_is_intra {
        (input, false)
    } else {
        trace_bool(input, ctx, "reference_select")?
    })
}

/// Parses skip-mode signaling after evaluating spec eligibility conditions.
#[allow(clippy::too_many_arguments)]
fn skip_mode_params<'a, 'b>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    frame_is_intra: bool,
    reference_select: bool,
    order_hint_bits: usize,
    order_hint: u64,
    ref_order_hint: &'b [u64],
    ref_frame_idx: &'b [usize],
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
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

            skip_mode_allowed = second_forward_idx >= 0;
        }
    }

    let (input, _skip_mode_present) = if skip_mode_allowed {
        trace_bool(input, ctx, "skip_mode_present")?
    } else {
        (input, false)
    };

    Ok((input, ()))
}

/// Computes wrapped signed distance between two order hints.
///
/// This matches AV1 modular arithmetic for picture order comparison.
#[must_use]
const fn get_relative_dist(a: i64, b: i64, order_hint_bits: usize) -> i64 {
    if order_hint_bits == 0 {
        return 0;
    }

    let diff = a - b;
    let m = 1 << (order_hint_bits - 1);
    (diff & (m - 1)) - (diff & m)
}

const GM_ABS_ALPHA_BITS: usize = 12;
const GM_ALPHA_PREC_BITS: usize = 15;
const GM_ABS_TRANS_ONLY_BITS: usize = 9;
const GM_TRANS_ONLY_PREC_BITS: usize = 3;
const GM_ABS_TRANS_BITS: usize = 12;
const GM_TRANS_PREC_BITS: usize = 6;
const WARPEDMODEL_PREC_BITS: usize = 16;
const IDENTITY: usize = 0;
const TRANSLATION: usize = 1;
const ROTZOOM: usize = 2;
const AFFINE: usize = 3;

/// Initializes global motion parameters to identity transforms.
#[must_use]
fn initialize_prev_gm_params() -> Vec<Vec<i32>> {
    let mut prev_gm_params = vec![vec![0i32; 6]; 8]; // Assuming 8 references and 6 indices

    for param_set in &mut prev_gm_params {
        for (i, param) in param_set.iter_mut().enumerate() {
            *param = if i % 3 == 2 {
                1i32 << WARPEDMODEL_PREC_BITS
            } else {
                0i32
            };
        }
    }

    prev_gm_params
}

/// Parses one global-motion parameter using subexponential coding.
///
/// The decoded value is interpreted relative to the previous parameter value
/// for the same reference frame and coefficient index.
fn read_global_param(
    input: BitInput,
    allow_high_precision_mv: bool,
    type_: usize,
    ref_: usize,
    idx: usize,
) -> IResult<BitInput, (), Error<BitInput>> {
    let mut abs_bits = GM_ABS_ALPHA_BITS;
    let mut prec_bits = GM_ALPHA_PREC_BITS;
    let mut gm_params = initialize_prev_gm_params();

    if idx < 2 {
        if type_ == TRANSLATION {
            abs_bits = GM_ABS_TRANS_ONLY_BITS - usize::from(!allow_high_precision_mv);
            prec_bits = GM_TRANS_ONLY_PREC_BITS - usize::from(!allow_high_precision_mv);
        } else {
            abs_bits = GM_ABS_TRANS_BITS;
            prec_bits = GM_TRANS_PREC_BITS;
        }
    }

    let prec_diff = WARPEDMODEL_PREC_BITS - prec_bits;
    let round = if idx % 3 == 2 {
        1i32 << WARPEDMODEL_PREC_BITS
    } else {
        0i32
    };
    let sub = if idx % 3 == 2 {
        1i32 << prec_bits
    } else {
        0i32
    };

    let mx = 1i32 << abs_bits;
    let r = (gm_params[ref_][idx] >> prec_diff) - sub;
    let (input, result) = decode_signed_subexp_with_ref(input, -mx, mx + 1, r)?;

    gm_params[ref_][idx] = (result << prec_diff) + round;

    Ok((input, ()))
}

/// Decodes a signed subexponential-coded value anchored around reference `r`.
fn decode_signed_subexp_with_ref(
    input: BitInput,
    low: i32,
    high: i32,
    r: i32,
) -> IResult<BitInput, i32, Error<BitInput>> {
    let (input, x) = decode_unsigned_subexp_with_ref(input, high - low, r - low)?;

    Ok((input, x + low))
}

/// Decodes an unsigned subexponential value and recenters it around `r`.
fn decode_unsigned_subexp_with_ref(
    input: BitInput,
    mx: i32,
    r: i32,
) -> IResult<BitInput, i32, Error<BitInput>> {
    let (input, v) = decode_subexp(input, mx)?;
    if (r << 1) <= mx {
        Ok((input, inverse_recenter(r, v)))
    } else {
        Ok((input, mx - 1 - inverse_recenter(mx - 1 - r, v)))
    }
}

/// Decodes an AV1 subexponential-coded integer in `[0, num_syms)`.
fn decode_subexp(input: BitInput, num_syms: i32) -> IResult<BitInput, i32, Error<BitInput>> {
    let mut i = 0i32;
    let mut mk = 0i32;
    let k = 3i32;

    let mut outer_input = input;
    loop {
        let mut input = outer_input;
        let b2 = if i != 0 { k + i - 1 } else { k };
        let a = 1 << b2;

        if num_syms <= mk + 3 * a {
            let (inner_input, subexp_final_bits) = ns(input, (num_syms - mk) as usize)?;
            input = inner_input;
            return Ok((input, subexp_final_bits as i32 + mk));
        }

        let (inner_input, subexp_more_bits) = take_bool_bit(input)?;
        input = inner_input;
        if subexp_more_bits {
            i += 1;
            mk += a;
        } else {
            let (inner_input, subexp_bits): (_, u8) = bit_parsers::take(b2 as u32)(input)?;
            input = inner_input;
            return Ok((input, i32::from(subexp_bits) + mk));
        }
        outer_input = input;
    }
}

/// Applies AV1 inverse recenter mapping.
#[must_use]
const fn inverse_recenter(r: i32, v: i32) -> i32 {
    if v > 2 * r {
        v
    } else if v & 1 == 1 {
        r - ((v + 1) >> 1)
    } else {
        r + (v >> 1)
    }
}

/// Parses global-motion model syntax for each inter reference frame.
fn global_motion_params<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx<'a>,
    frame_is_intra: bool,
    allow_high_precision_mv: bool,
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
    if frame_is_intra {
        return Ok((input, ()));
    }

    let mut input = input;

    for ref_ in (RefType::Last as u8)..=(RefType::Altref as u8) {
        let mut type_ = IDENTITY;

        let (inner_input, is_global) = trace_bool(input, ctx, &format!("is_global[{ref_}]"))?;
        input = inner_input;
        if is_global {
            let (inner_input, is_rot_zoom) =
                trace_bool(input, ctx, &format!("is_rot_zoom[{ref_}]"))?;
            input = inner_input;
            if is_rot_zoom {
                type_ = ROTZOOM;
            } else {
                let (inner_input, is_translation) =
                    trace_bool(input, ctx, &format!("is_translation[{ref_}]"))?;
                input = inner_input;
                if is_translation {
                    type_ = TRANSLATION;
                } else {
                    type_ = AFFINE;
                }
            }
        }

        if type_ >= ROTZOOM {
            let (inner_input, _) =
                read_global_param(input, allow_high_precision_mv, type_, ref_ as usize, 2)?;
            input = inner_input;
            let (inner_input, _) =
                read_global_param(input, allow_high_precision_mv, type_, ref_ as usize, 3)?;
            input = inner_input;

            if type_ == AFFINE {
                let (inner_input, _) =
                    read_global_param(input, allow_high_precision_mv, type_, ref_ as usize, 4)?;
                input = inner_input;
                let (inner_input, _) =
                    read_global_param(input, allow_high_precision_mv, type_, ref_ as usize, 5)?;
                input = inner_input;
            }
        }
        if type_ >= TRANSLATION {
            let (inner_input, _) =
                read_global_param(input, allow_high_precision_mv, type_, ref_ as usize, 0)?;
            input = inner_input;
            let (inner_input, _) =
                read_global_param(input, allow_high_precision_mv, type_, ref_ as usize, 1)?;
            input = inner_input;
        }
    }

    Ok((input, ()))
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

/// Resolves the active quantizer index for a segment.
///
/// This applies segmentation `ALT_Q` offsets and optionally uses
/// `current_q_index` when delta-q is active.
#[must_use]
fn get_qindex(
    ignore_delta_q: bool,
    segment_id: usize,
    base_q_idx: u8,
    current_q_index: Option<u8>,
    feature_data: Option<&SegmentationData>,
) -> u8 {
    if seg_feature_active_idx(segment_id, SEG_LVL_ALT_Q, feature_data) {
        let data = feature_data.unwrap()[segment_id][SEG_LVL_ALT_Q].unwrap();
        let qindex = if !ignore_delta_q && let Some(current_q_index) = current_q_index {
            i16::from(current_q_index) + data
        } else {
            i16::from(base_q_idx) + data
        };
        return clamp(qindex, 0, 255) as u8;
    } else if !ignore_delta_q
        && current_q_index.is_some()
        && let Some(current_q_index) = current_q_index
    {
        return current_q_index;
    }
    base_q_idx
}

/// Returns whether a segmentation feature is enabled for `segment_id`.
#[must_use]
const fn seg_feature_active_idx(
    segment_id: usize,
    feature: usize,
    feature_data: Option<&SegmentationData>,
) -> bool {
    if let Some(feature_data) = feature_data {
        feature_data[segment_id][feature].is_some()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        trace::TraceCtx,
        util::{BitInput, ns, su},
    };
    use super::{
        Dimensions, FrameType, REFS_PER_FRAME, WARPEDMODEL_PREC_BITS, cdef_params,
        compute_image_size, decode_signed_subexp_with_ref, decode_subexp,
        decode_unsigned_subexp_with_ref, delta_lf_params, delta_q_params, frame_reference_mode,
        frame_size, frame_size_with_refs, get_qindex, get_relative_dist, global_motion_params,
        initialize_prev_gm_params, inverse_recenter, loop_filter_params, lr_params,
        quantization_params, read_delta_q, read_interpolation_filter, read_tx_mode, render_size,
        seg_feature_active_idx, segmentation_params, skip_mode_params, superres_params,
        temporal_point_info, tile_info, tile_log2,
    };

    fn test_ctx(input: BitInput) -> TraceCtx {
        TraceCtx::new(input, 0)
    }

    // -----------------------------------------------------------------------
    // Test infrastructure
    // -----------------------------------------------------------------------

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

        /// Encodes a signed value in two's complement `n` bits (matching AV1 `su(n)`).
        fn push_su(&mut self, value: i64, width: usize) {
            let mask = (1u64 << width) - 1;
            let encoded = (value as u64) & mask;
            self.push_bits(encoded, width);
        }

        /// Encodes `value` using AV1 `ns(n)` non-symmetric coding.
        fn push_ns(&mut self, value: u64, n: usize) {
            let w = {
                let mut s = 0usize;
                let mut x = n;
                while x != 0 {
                    x >>= 1;
                    s += 1;
                }
                s
            };
            let m = ((1usize << w) - n) as u64;
            if value < m {
                self.push_bits(value, w - 1);
            } else {
                let encoded = value + m;
                self.push_bits(encoded, w);
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

    fn assert_remaining_position(remaining: BitInput, input: &[u8], consumed_bits: usize) {
        assert_eq!(remaining.0, &input[consumed_bits / 8..]);
        assert_eq!(remaining.1, consumed_bits % 8);
    }

    /// Verifies that `push_ns` round-trips through the `ns` parser.
    #[test]
    fn push_ns_roundtrips_through_ns_parser() {
        for n in [2, 3, 5, 8, 10, 16, 100] {
            for v in 0..n as u64 {
                let mut bits = BitBuilder::default();
                bits.push_ns(v, n);
                let (data, consumed) = with_trailer(bits);
                let (rem, decoded) = ns((&data, 0), n).unwrap();
                assert_eq!(decoded, v, "ns roundtrip failed for n={n}, v={v}");
                assert_remaining_position(rem, &data, consumed);
            }
        }
    }

    /// Verifies that `push_su` round-trips through the `su` parser.
    #[test]
    fn push_su_roundtrips_through_su_parser() {
        for width in [1, 4, 7, 9] {
            let min = -(1i64 << (width - 1));
            let max = (1i64 << (width - 1)) - 1;
            for v in [min, min + 1, 0, max - 1, max] {
                let mut bits = BitBuilder::default();
                bits.push_su(v, width);
                let (data, consumed) = with_trailer(bits);
                let (rem, decoded) = su((&data, 0), width).unwrap();
                assert_eq!(decoded, v, "su roundtrip failed for width={width}, v={v}");
                assert_remaining_position(rem, &data, consumed);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Group 1: FrameType::is_intra
    // -----------------------------------------------------------------------

    #[test]
    fn is_intra_true_for_key() {
        assert!(FrameType::Key.is_intra());
    }

    #[test]
    fn is_intra_true_for_intra_only() {
        assert!(FrameType::IntraOnly.is_intra());
    }

    #[test]
    fn is_intra_false_for_inter() {
        assert!(!FrameType::Inter.is_intra());
    }

    #[test]
    fn is_intra_false_for_switch() {
        assert!(!FrameType::Switch.is_intra());
    }

    // -----------------------------------------------------------------------
    // Group 2: temporal_point_info
    // -----------------------------------------------------------------------

    #[test]
    fn temporal_point_info_consumes_exact_bits() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0xABCD, 16);
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = temporal_point_info(input, test_ctx(input), 16).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn temporal_point_info_consumes_one_bit() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true);
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = temporal_point_info(input, test_ctx(input), 1).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn temporal_point_info_consumes_zero_bits() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = temporal_point_info(input, test_ctx(input), 0).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 3: superres_params
    // -----------------------------------------------------------------------

    #[test]
    fn superres_disabled_identity() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let mut fs = Dimensions {
            width: 1920,
            height: 1080,
        };
        let mut us = fs;
        let input: BitInput = (&data, 0);
        let (rem, _) = superres_params(input, test_ctx(input), false, &mut fs, &mut us).unwrap();
        assert_eq!(fs.width, 1920);
        assert_eq!(us.width, 1920);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn superres_enabled_but_not_used() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // use_superres = false
        let (data, consumed) = with_trailer(bits);
        let mut fs = Dimensions {
            width: 1920,
            height: 1080,
        };
        let mut us = fs;
        let input: BitInput = (&data, 0);
        let (rem, _) = superres_params(input, test_ctx(input), true, &mut fs, &mut us).unwrap();
        // denom=8, width unchanged: (1920*8+4)/8 = 1920
        assert_eq!(fs.width, 1920);
        assert_eq!(us.width, 1920);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn superres_enabled_and_used_denom_9() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // use_superres = true
        bits.push_bits(0, 3); // coded_denom = 0 → denom = 9
        let (data, consumed) = with_trailer(bits);
        let mut fs = Dimensions {
            width: 1920,
            height: 1080,
        };
        let mut us = fs;
        let input: BitInput = (&data, 0);
        let (rem, _) = superres_params(input, test_ctx(input), true, &mut fs, &mut us).unwrap();
        // width = (1920*8 + 4) / 9 = 15364 / 9 = 1707
        assert_eq!(fs.width, (1920 * 8 + 4) / 9);
        assert_eq!(us.width, 1920);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn superres_enabled_and_used_max_denom() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // use_superres = true
        bits.push_bits(7, 3); // coded_denom = 7 → denom = 16
        let (data, consumed) = with_trailer(bits);
        let mut fs = Dimensions {
            width: 1920,
            height: 1080,
        };
        let mut us = fs;
        let input: BitInput = (&data, 0);
        let (rem, _) = superres_params(input, test_ctx(input), true, &mut fs, &mut us).unwrap();
        // width = (1920*8 + 8) / 16 = 15368 / 16 = 960
        assert_eq!(fs.width, (1920 * 8 + 8) / 16);
        assert_eq!(us.width, 1920);
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 4: frame_size
    // -----------------------------------------------------------------------

    #[test]
    fn frame_size_no_override_uses_max() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let max = Dimensions {
            width: 1920,
            height: 1080,
        };
        let input: BitInput = (&data, 0);
        let (rem, fs) = frame_size(input, test_ctx(input), false, false, 11, 11, max).unwrap();
        assert_eq!(fs.width, 1920);
        assert_eq!(fs.height, 1080);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn frame_size_override_reads_dims() {
        let mut bits = BitBuilder::default();
        bits.push_bits(959, 11); // width_minus_1 = 959 → width = 960
        bits.push_bits(539, 11); // height_minus_1 = 539 → height = 540
        let (data, consumed) = with_trailer(bits);
        let max = Dimensions {
            width: 1920,
            height: 1080,
        };
        let input: BitInput = (&data, 0);
        let (rem, fs) = frame_size(input, test_ctx(input), true, false, 11, 11, max).unwrap();
        assert_eq!(fs.width, 960);
        assert_eq!(fs.height, 540);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn frame_size_override_with_superres() {
        let mut bits = BitBuilder::default();
        bits.push_bits(1919, 11); // width_minus_1 = 1919 → width = 1920
        bits.push_bits(1079, 11); // height_minus_1 = 1079 → height = 1080
        bits.push_bool(true); // use_superres = true
        bits.push_bits(0, 3); // coded_denom = 0 → denom = 9
        let (data, consumed) = with_trailer(bits);
        let max = Dimensions {
            width: 1920,
            height: 1080,
        };
        let input: BitInput = (&data, 0);
        let (rem, fs) = frame_size(input, test_ctx(input), true, true, 11, 11, max).unwrap();
        assert_eq!(fs.width, (1920 * 8 + 4) / 9);
        assert_eq!(fs.height, 1080);
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 5: render_size
    // -----------------------------------------------------------------------

    #[test]
    fn render_size_same_as_frame() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // render_and_frame_size_different = false
        let (data, consumed) = with_trailer(bits);
        let fs = Dimensions {
            width: 1920,
            height: 1080,
        };
        let us = Dimensions {
            width: 1920,
            height: 1080,
        };
        let input: BitInput = (&data, 0);
        let (rem, rs) = render_size(input, test_ctx(input), fs, us).unwrap();
        assert_eq!(rs.width, 1920);
        assert_eq!(rs.height, 1080);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn render_size_different_reads_32_bits() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // render_and_frame_size_different = true
        bits.push_bits(1279, 16); // render_width_minus_1 = 1279 → 1280
        bits.push_bits(719, 16); // render_height_minus_1 = 719 → 720
        let (data, consumed) = with_trailer(bits);
        let fs = Dimensions {
            width: 1920,
            height: 1080,
        };
        let us = Dimensions {
            width: 1920,
            height: 1080,
        };
        let input: BitInput = (&data, 0);
        let (rem, rs) = render_size(input, test_ctx(input), fs, us).unwrap();
        assert_eq!(rs.width, 1280);
        assert_eq!(rs.height, 720);
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 6: frame_size_with_refs
    // -----------------------------------------------------------------------

    #[test]
    fn frame_size_with_refs_found_ref_first_iter() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // found_ref on first iter
        // superres disabled (enable_superres=false): no bits needed
        let (data, consumed) = with_trailer(bits);
        let max = Dimensions {
            width: 1920,
            height: 1080,
        };
        let mut ref_fs = Dimensions {
            width: 1280,
            height: 720,
        };
        let mut ref_us = ref_fs;
        let input: BitInput = (&data, 0);
        let (rem, fs) = frame_size_with_refs(
            input,
            test_ctx(input),
            false,
            true,
            11,
            11,
            max,
            &mut ref_fs,
            &mut ref_us,
        )
        .unwrap();
        assert_eq!(fs.width, 1280);
        assert_eq!(fs.height, 720);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn frame_size_with_refs_found_ref_third_iter() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // 1st: not found
        bits.push_bool(false); // 2nd: not found
        bits.push_bool(true); // 3rd: found
        let (data, consumed) = with_trailer(bits);
        let max = Dimensions {
            width: 1920,
            height: 1080,
        };
        let mut ref_fs = Dimensions {
            width: 640,
            height: 480,
        };
        let mut ref_us = ref_fs;
        let input: BitInput = (&data, 0);
        let (rem, fs) = frame_size_with_refs(
            input,
            test_ctx(input),
            false,
            true,
            11,
            11,
            max,
            &mut ref_fs,
            &mut ref_us,
        )
        .unwrap();
        assert_eq!(fs.width, 640);
        assert_eq!(fs.height, 480);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn frame_size_with_refs_no_ref_found_falls_back() {
        let mut bits = BitBuilder::default();
        // 7 false flags (no ref found)
        for _ in 0..REFS_PER_FRAME {
            bits.push_bool(false);
        }
        // Falls back to frame_size: override=true, enable_superres=false
        bits.push_bits(959, 11); // width_minus_1 = 959 → 960
        bits.push_bits(539, 11); // height_minus_1 = 539 → 540
        // Then render_size: same
        bits.push_bool(false);
        let (data, consumed) = with_trailer(bits);
        let max = Dimensions {
            width: 1920,
            height: 1080,
        };
        let mut ref_fs = Dimensions {
            width: 1920,
            height: 1080,
        };
        let mut ref_us = ref_fs;
        let input: BitInput = (&data, 0);
        let (rem, fs) = frame_size_with_refs(
            input,
            test_ctx(input),
            false,
            true,
            11,
            11,
            max,
            &mut ref_fs,
            &mut ref_us,
        )
        .unwrap();
        assert_eq!(fs.width, 960);
        assert_eq!(fs.height, 540);
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 7: compute_image_size
    // -----------------------------------------------------------------------

    #[test]
    fn compute_image_size_standard_1080p() {
        let (mi_cols, mi_rows) = compute_image_size(Dimensions {
            width: 1920,
            height: 1080,
        });
        assert_eq!(mi_cols, 480);
        assert_eq!(mi_rows, 270);
    }

    #[test]
    fn compute_image_size_rounds_up() {
        let (mi_cols, mi_rows) = compute_image_size(Dimensions {
            width: 1,
            height: 1,
        });
        assert_eq!(mi_cols, 2);
        assert_eq!(mi_rows, 2);
    }

    #[test]
    fn compute_image_size_exact_alignment() {
        let (mi_cols, mi_rows) = compute_image_size(Dimensions {
            width: 8,
            height: 16,
        });
        assert_eq!(mi_cols, 2);
        assert_eq!(mi_rows, 4);
    }

    #[test]
    fn compute_image_size_zero_dimensions() {
        let (mi_cols, mi_rows) = compute_image_size(Dimensions {
            width: 0,
            height: 0,
        });
        assert_eq!(mi_cols, 0);
        assert_eq!(mi_rows, 0);
    }

    // -----------------------------------------------------------------------
    // Group 8: read_interpolation_filter
    // -----------------------------------------------------------------------

    #[test]
    fn read_interpolation_filter_switchable() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // is_filter_switchable
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = read_interpolation_filter(input, test_ctx(input)).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn read_interpolation_filter_explicit() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // not switchable
        bits.push_bits(2, 2); // explicit filter index
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = read_interpolation_filter(input, test_ctx(input)).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 9: tile_log2
    // -----------------------------------------------------------------------

    #[test]
    fn tile_log2_equal() {
        assert_eq!(tile_log2(4u32, 4u32), 0);
    }

    #[test]
    fn tile_log2_exceeds() {
        assert_eq!(tile_log2(8u32, 4u32), 0);
    }

    #[test]
    fn tile_log2_one_shift() {
        assert_eq!(tile_log2(4u32, 5u32), 1);
    }

    #[test]
    fn tile_log2_multiple_shifts() {
        assert_eq!(tile_log2(1u32, 16u32), 4);
    }

    #[test]
    fn tile_log2_both_one() {
        assert_eq!(tile_log2(1u32, 1u32), 0);
    }

    // -----------------------------------------------------------------------
    // Group 10: tile_info
    // -----------------------------------------------------------------------

    #[test]
    fn tile_info_uniform_single_tile_64() {
        // Small frame with 64×64 superblocks → single tile
        // 32×32 pixels → mi_cols=8, mi_rows=8
        // sb_cols=(8+15)>>4=1, sb_rows=1
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // uniform_tile_spacing
        // min_log2_tile_cols=tile_log2(64,1)=0, max_log2_tile_cols=tile_log2(1,min(1,64))=0
        // So no increment bits for cols.
        // min_log2_tile_rows=0, max_log2_tile_rows=0
        // So no increment bits for rows.
        // tile_cols_log2=0, tile_rows_log2=0 → no context_update/size_bytes
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, ti) = tile_info(input, test_ctx(input), false, 8, 8).unwrap();
        assert_eq!(ti.tile_cols, 1);
        assert_eq!(ti.tile_rows, 1);
        assert_eq!(ti.tile_cols_log2, 0);
        assert_eq!(ti.tile_rows_log2, 0);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn tile_info_uniform_multi_tile() {
        // Larger frame: 4096×2160 → mi_cols=1024, mi_rows=544
        // 64×64 SB: sb_cols=(1024+15)>>4=64, sb_rows=(544+15)>>4=34
        // max_tile_width_sb = 4096 >> 6 = 64
        // min_log2_tile_cols = tile_log2(64,64)=0
        // max_log2_tile_cols = tile_log2(1,min(64,64))=6
        // Write uniform=true, then increment cols once
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // uniform
        bits.push_bool(true); // increment tile_cols_log2 to 1
        bits.push_bool(false); // stop incrementing cols
        // Now tile_cols_log2=1
        // tile_width_sb=(64+(1<<1)-1)>>1=32, tile_cols=64/32=2
        // min_log2_tiles = max(0, tile_log2(max_tile_area_sb, sb_rows*sb_cols))
        // max_tile_area_sb = 4096*2304 >> 12 = 2304
        // sb_rows*sb_cols = 64*34 = 2176
        // tile_log2(2304, 2176) = 0
        // min_log2_tile_rows = max(0 - 1, 0) = 0
        // max_log2_tile_rows = tile_log2(1, min(34,64)) = 6 (since 1<<5=32 < 34)
        bits.push_bool(false); // don't increment rows
        // tile_rows_log2=0
        // tile_height_sb = (34 + 0) >> 0 = 34, tile_rows = 34/34 = 1
        // tile_cols_log2(1) + tile_rows_log2(0) = 1 > 0 → reads context_update + size_bytes
        bits.push_bits(0, 1); // context_update_tile_id (1 bit = tile_cols_log2 + tile_rows_log2)
        bits.push_bits(0, 2); // tile_size_bytes_minus_1
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, ti) = tile_info(input, test_ctx(input), false, 1024, 544).unwrap();
        assert_eq!(ti.tile_cols, 2);
        assert_eq!(ti.tile_rows, 1);
        assert_eq!(ti.tile_cols_log2, 1);
        assert_eq!(ti.tile_rows_log2, 0);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn tile_info_non_uniform_small_frame() {
        // Small frame: mi_cols=8, mi_rows=8 with 64×64 SB
        // sb_cols=1, sb_rows=1
        // Non-uniform: reads ns() widths/heights
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // non-uniform
        // max_tile_width_sb = 64
        // First col iteration: start_sb=0, max_width=min(1-0, 64)=1
        // ns(1) reads 0 bits, returns 0 → size_sb=1, start_sb=1 → loop exits
        // tile_cols=1
        // widest_tile_sb=1, max_tile_height_sb=max(4096*2304/64/1, 1) huge
        // First row iteration: max_height=min(1-0, huge)=1
        // ns(1) reads 0 bits, returns 0 → size_sb=1, start_sb=1 → loop exits
        // tile_rows=1
        // tile_cols_log2=0, tile_rows_log2=0 → no context/size bits
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, ti) = tile_info(input, test_ctx(input), false, 8, 8).unwrap();
        assert_eq!(ti.tile_cols, 1);
        assert_eq!(ti.tile_rows, 1);
        assert_eq!(ti.tile_cols_log2, 0);
        assert_eq!(ti.tile_rows_log2, 0);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn tile_info_uniform_128x128_sb() {
        // 1920×1080 with 128×128 SB
        // mi_cols=480, mi_rows=272
        // sb_cols=(480+31)>>5=15, sb_rows=(272+31)>>5=9
        // sb_shift=5, sb_size=7
        // max_tile_width_sb=4096>>7=32
        // min_log2_tile_cols=tile_log2(32,15)=0
        // max_log2_tile_cols=tile_log2(1,min(15,64))=4 (1<<3=8<15, 1<<4=16>=15)
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // uniform
        bits.push_bool(false); // don't increment cols → tile_cols_log2=0
        // tile_width_sb=(15+0)>>0=15, tile_cols=15/15=1
        // max_tile_area_sb=4096*2304>>14=576
        // min_log2_tiles=max(0, tile_log2(576, 15*9))=tile_log2(576, 135)=0
        // min_log2_tile_rows=max(0-0, 0)=0
        // max_log2_tile_rows=tile_log2(1, min(9,64))=4 (1<<3=8<9, 1<<4=16>=9)
        bits.push_bool(false); // don't increment rows → tile_rows_log2=0
        // tile_cols_log2=0 + tile_rows_log2=0 = 0 → no context/size bits
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, ti) = tile_info(input, test_ctx(input), true, 480, 272).unwrap();
        assert_eq!(ti.tile_cols, 1);
        assert_eq!(ti.tile_rows, 1);
        assert_eq!(ti.tile_cols_log2, 0);
        assert_eq!(ti.tile_rows_log2, 0);
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 11: quantization_params
    // -----------------------------------------------------------------------

    #[test]
    fn quantization_params_mono_no_qmatrix() {
        let mut bits = BitBuilder::default();
        bits.push_bits(128, 8); // base_q_idx = 128
        bits.push_bool(false); // deltaq_y_dc not coded
        // num_planes=1 → skip U/V deltas
        bits.push_bool(false); // using_qmatrix = false
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, qp) = quantization_params(input, test_ctx(input), 1, false).unwrap();
        assert_eq!(qp.base_q_idx, 128);
        assert_eq!(qp.deltaq_y_dc, 0);
        assert_eq!(qp.deltaq_u_dc, 0);
        assert_eq!(qp.deltaq_u_ac, 0);
        assert_eq!(qp.deltaq_v_dc, 0);
        assert_eq!(qp.deltaq_v_ac, 0);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn quantization_params_multi_plane_no_separate_uv() {
        let mut bits = BitBuilder::default();
        bits.push_bits(100, 8); // base_q_idx
        bits.push_bool(false); // deltaq_y_dc not coded
        // num_planes=3, separate_uv=false → diff_uv_delta=false
        bits.push_bool(false); // deltaq_u_dc not coded
        bits.push_bool(false); // deltaq_u_ac not coded
        // V copies U (diff_uv_delta=false)
        bits.push_bool(false); // using_qmatrix = false
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, qp) = quantization_params(input, test_ctx(input), 3, false).unwrap();
        assert_eq!(qp.base_q_idx, 100);
        assert_eq!(qp.deltaq_v_dc, qp.deltaq_u_dc);
        assert_eq!(qp.deltaq_v_ac, qp.deltaq_u_ac);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn quantization_params_separate_uv_diff_delta() {
        let mut bits = BitBuilder::default();
        bits.push_bits(50, 8); // base_q_idx
        bits.push_bool(false); // deltaq_y_dc not coded
        // separate_uv=true
        bits.push_bool(true); // diff_uv_delta = true
        bits.push_bool(true); // deltaq_u_dc coded
        bits.push_su(10, 7); // deltaq_u_dc = 10
        bits.push_bool(false); // deltaq_u_ac not coded
        // diff_uv_delta=true → read V independently
        bits.push_bool(true); // deltaq_v_dc coded
        bits.push_su(-5, 7); // deltaq_v_dc = -5
        bits.push_bool(false); // deltaq_v_ac not coded
        bits.push_bool(false); // using_qmatrix = false
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, qp) = quantization_params(input, test_ctx(input), 3, true).unwrap();
        assert_eq!(qp.deltaq_u_dc, 10);
        assert_eq!(qp.deltaq_v_dc, -5);
        assert_eq!(qp.deltaq_u_ac, 0);
        assert_eq!(qp.deltaq_v_ac, 0);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn quantization_params_separate_uv_no_diff() {
        let mut bits = BitBuilder::default();
        bits.push_bits(50, 8); // base_q_idx
        bits.push_bool(false); // deltaq_y_dc
        bits.push_bool(false); // diff_uv_delta = false
        bits.push_bool(true); // deltaq_u_dc coded
        bits.push_su(7, 7); // deltaq_u_dc = 7
        bits.push_bool(false); // deltaq_u_ac
        // diff_uv_delta=false → V = U
        bits.push_bool(false); // using_qmatrix
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, qp) = quantization_params(input, test_ctx(input), 3, true).unwrap();
        assert_eq!(qp.deltaq_u_dc, 7);
        assert_eq!(qp.deltaq_v_dc, 7);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn quantization_params_qmatrix_separate_uv() {
        let mut bits = BitBuilder::default();
        bits.push_bits(128, 8); // base_q_idx
        bits.push_bool(false); // deltaq_y_dc
        bits.push_bool(false); // diff_uv_delta = false (separate_uv=true)
        bits.push_bool(false); // deltaq_u_dc
        bits.push_bool(false); // deltaq_u_ac
        bits.push_bool(true); // using_qmatrix = true
        bits.push_bits(5, 4); // qm_y
        bits.push_bits(3, 4); // qm_u
        bits.push_bits(7, 4); // qm_v (separate_uv=true → reads independently)
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = quantization_params(input, test_ctx(input), 3, true).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn quantization_params_qmatrix_shared_uv() {
        let mut bits = BitBuilder::default();
        bits.push_bits(128, 8);
        bits.push_bool(false);
        // separate_uv=false → no diff_uv_delta bit
        bits.push_bool(false);
        bits.push_bool(false);
        bits.push_bool(true); // using_qmatrix
        bits.push_bits(5, 4); // qm_y
        bits.push_bits(3, 4); // qm_u; qm_v = qm_u (no extra read)
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = quantization_params(input, test_ctx(input), 3, false).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 12: read_delta_q
    // -----------------------------------------------------------------------

    #[test]
    fn read_delta_q_not_coded_zero() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // delta_coded = false
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, val) = read_delta_q(input, test_ctx(input), "test").unwrap();
        assert_eq!(val, 0);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn read_delta_q_coded_positive() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // delta_coded = true
        bits.push_su(42, 7); // su(7) = 42
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, val) = read_delta_q(input, test_ctx(input), "test").unwrap();
        assert_eq!(val, 42);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn read_delta_q_coded_negative() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true);
        bits.push_su(-10, 7);
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, val) = read_delta_q(input, test_ctx(input), "test").unwrap();
        assert_eq!(val, -10);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn read_delta_q_coded_zero() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true);
        bits.push_su(0, 7);
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, val) = read_delta_q(input, test_ctx(input), "test").unwrap();
        assert_eq!(val, 0);
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 13: segmentation_params
    // -----------------------------------------------------------------------

    #[test]
    fn segmentation_params_disabled_returns_none() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // segmentation_enabled = false
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, result) = segmentation_params(input, test_ctx(input), 7).unwrap();
        assert!(result.is_none());
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn segmentation_params_primary_ref_none_no_features() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // segmentation_enabled
        // primary_ref_frame=7 (PRIMARY_REF_NONE) → segmentation_update_data=true
        // 8 segments × 8 features = 64 feature_enabled flags, all false
        for _ in 0..64 {
            bits.push_bool(false);
        }
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, result) = segmentation_params(input, test_ctx(input), 7).unwrap();
        let seg_data = result.unwrap();
        for seg in &seg_data {
            for feat in seg {
                assert!(feat.is_none());
            }
        }
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn segmentation_params_primary_ref_none_unsigned_feature() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // segmentation_enabled
        // Segment 0, features 0-4: all disabled, feature 5: enabled
        for _ in 0..5 {
            bits.push_bool(false);
        }
        bits.push_bool(true); // feature 5 enabled (unsigned, 3 bits, max=7)
        bits.push_bits(5, 3); // feature value = 5
        bits.push_bool(false); // feature 6
        bits.push_bool(false); // feature 7
        // Segments 1-7: all features disabled
        for _ in 0..(7 * 8) {
            bits.push_bool(false);
        }
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, result) = segmentation_params(input, test_ctx(input), 7).unwrap();
        let seg_data = result.unwrap();
        assert_eq!(seg_data[0][5], Some(5));
        assert!(seg_data[0][0].is_none());
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn segmentation_params_primary_ref_none_signed_feature() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // segmentation_enabled
        // Segment 0, feature 0: enabled (signed, 8+1=9 bits including sign, su(1+8))
        bits.push_bool(true); // feature 0 enabled
        bits.push_su(-50, 9); // feature value = -50 (SEGMENTATION_FEATURE_BITS[0]=8, signed → su(1+8))
        // Features 1-7: disabled
        for _ in 1..8 {
            bits.push_bool(false);
        }
        // Segments 1-7: all disabled
        for _ in 0..(7 * 8) {
            bits.push_bool(false);
        }
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, result) = segmentation_params(input, test_ctx(input), 7).unwrap();
        let seg_data = result.unwrap();
        assert_eq!(seg_data[0][0], Some(-50));
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn segmentation_params_non_primary_update_map_and_data() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // segmentation_enabled
        // primary_ref_frame=0 (not PRIMARY_REF_NONE)
        bits.push_bool(true); // segmentation_update_map
        bits.push_bool(true); // segmentation_temporal_update
        bits.push_bool(true); // segmentation_update_data
        // 8 segments × 8 features, all disabled
        for _ in 0..64 {
            bits.push_bool(false);
        }
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, result) = segmentation_params(input, test_ctx(input), 0).unwrap();
        assert!(result.is_some());
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn segmentation_params_non_primary_no_update_data() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // segmentation_enabled
        bits.push_bool(false); // segmentation_update_map = false
        bits.push_bool(false); // segmentation_update_data = false
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, result) = segmentation_params(input, test_ctx(input), 0).unwrap();
        assert!(result.is_some());
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn segmentation_params_non_primary_no_map_but_data() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // segmentation_enabled
        bits.push_bool(false); // segmentation_update_map = false
        bits.push_bool(true); // segmentation_update_data = true
        // 64 feature flags, all false
        for _ in 0..64 {
            bits.push_bool(false);
        }
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, result) = segmentation_params(input, test_ctx(input), 0).unwrap();
        assert!(result.is_some());
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 14: delta_q_params
    // -----------------------------------------------------------------------

    #[test]
    fn delta_q_params_base_zero_returns_false() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, present) = delta_q_params(input, test_ctx(input), 0).unwrap();
        assert!(!present);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn delta_q_params_nonzero_present_true() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // delta_q_present = true
        bits.push_bits(2, 2); // delta_q_res = 2
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, present) = delta_q_params(input, test_ctx(input), 100).unwrap();
        assert!(present);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn delta_q_params_nonzero_present_false() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // delta_q_present = false
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, present) = delta_q_params(input, test_ctx(input), 100).unwrap();
        assert!(!present);
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 15: delta_lf_params
    // -----------------------------------------------------------------------

    #[test]
    fn delta_lf_params_delta_q_not_present() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = delta_lf_params(input, test_ctx(input), false, false).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn delta_lf_params_intrabc_forces_false() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = delta_lf_params(input, test_ctx(input), true, true).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn delta_lf_params_lf_present_true() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // delta_lf_present = true
        bits.push_bits(1, 2); // delta_lf_res
        bits.push_bool(false); // delta_lf_multi
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = delta_lf_params(input, test_ctx(input), true, false).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn delta_lf_params_lf_present_false() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // delta_lf_present = false
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = delta_lf_params(input, test_ctx(input), true, false).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 16: loop_filter_params
    // -----------------------------------------------------------------------

    #[test]
    fn loop_filter_params_coded_lossless_early_return() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = loop_filter_params(input, test_ctx(input), true, false, 3).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn loop_filter_params_intrabc_early_return() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = loop_filter_params(input, test_ctx(input), false, true, 3).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn loop_filter_params_mono_both_zero() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 6); // l0 = 0
        bits.push_bits(0, 6); // l1 = 0
        // num_planes=1, l0=l1=0 → skip l2/l3
        bits.push_bits(2, 3); // sharpness
        bits.push_bool(false); // delta_enabled = false
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = loop_filter_params(input, test_ctx(input), false, false, 1).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn loop_filter_params_multi_plane_nonzero() {
        let mut bits = BitBuilder::default();
        bits.push_bits(10, 6); // l0 = 10
        bits.push_bits(5, 6); // l1 = 5
        // num_planes=3, l0>0 → reads l2/l3
        bits.push_bits(3, 6); // l2
        bits.push_bits(7, 6); // l3
        bits.push_bits(4, 3); // sharpness
        bits.push_bool(false); // delta_enabled = false
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = loop_filter_params(input, test_ctx(input), false, false, 3).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn loop_filter_params_multi_plane_l0_zero_l1_nonzero() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 6); // l0 = 0
        bits.push_bits(5, 6); // l1 = 5 (nonzero → reads l2/l3)
        bits.push_bits(1, 6); // l2
        bits.push_bits(2, 6); // l3
        bits.push_bits(0, 3); // sharpness
        bits.push_bool(false); // delta_enabled
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = loop_filter_params(input, test_ctx(input), false, false, 3).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn loop_filter_params_delta_enabled_with_update() {
        let mut bits = BitBuilder::default();
        bits.push_bits(10, 6); // l0
        bits.push_bits(0, 6); // l1
        // num_planes=1, l0>0 but num_planes==1 → no l2/l3 is wrong,
        // let's use num_planes=1 with l0>0: the condition is num_planes > 1 AND (l0>0 || l1>0)
        // So with num_planes=1 we skip l2/l3
        bits.push_bits(3, 3); // sharpness
        bits.push_bool(true); // delta_enabled
        bits.push_bool(true); // delta_update
        // 8 ref deltas: first one updates, rest don't
        bits.push_bool(true); // update_ref_delta[0]
        bits.push_su(5, 7); // delta value
        for _ in 1..8 {
            bits.push_bool(false); // no update
        }
        // 2 mode deltas: none update
        bits.push_bool(false);
        bits.push_bool(false);
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = loop_filter_params(input, test_ctx(input), false, false, 1).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn loop_filter_params_delta_enabled_no_update() {
        let mut bits = BitBuilder::default();
        bits.push_bits(10, 6); // l0
        bits.push_bits(0, 6); // l1
        bits.push_bits(3, 3); // sharpness
        bits.push_bool(true); // delta_enabled
        bits.push_bool(false); // delta_update = false
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = loop_filter_params(input, test_ctx(input), false, false, 1).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 17: cdef_params
    // -----------------------------------------------------------------------

    #[test]
    fn cdef_params_lossless_early_return() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = cdef_params(input, test_ctx(input), true, false, true, 3).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn cdef_params_intrabc_early_return() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = cdef_params(input, test_ctx(input), false, true, true, 3).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn cdef_params_disabled_early_return() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = cdef_params(input, test_ctx(input), false, false, false, 3).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn cdef_params_mono_bits_zero() {
        let mut bits = BitBuilder::default();
        bits.push_bits(1, 2); // cdef_damping_minus_3
        bits.push_bits(0, 2); // cdef_bits = 0 → 1 iteration
        // 1 iteration, mono: y_pri(4) + y_sec(2) = 6
        bits.push_bits(5, 4); // cdef_y_pri_str
        bits.push_bits(1, 2); // cdef_y_sec_str
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = cdef_params(input, test_ctx(input), false, false, true, 1).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn cdef_params_multi_plane_bits_one() {
        let mut bits = BitBuilder::default();
        bits.push_bits(2, 2); // cdef_damping_minus_3
        bits.push_bits(1, 2); // cdef_bits = 1 → 2 iterations
        for _ in 0..2 {
            bits.push_bits(3, 4); // y_pri
            bits.push_bits(1, 2); // y_sec
            bits.push_bits(2, 4); // uv_pri
            bits.push_bits(0, 2); // uv_sec
        }
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = cdef_params(input, test_ctx(input), false, false, true, 3).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn cdef_params_multi_plane_bits_three() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 2); // damping
        bits.push_bits(3, 2); // cdef_bits = 3 → 8 iterations
        for _ in 0..8 {
            bits.push_bits(0, 4); // y_pri
            bits.push_bits(0, 2); // y_sec
            bits.push_bits(0, 4); // uv_pri
            bits.push_bits(0, 2); // uv_sec
        }
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = cdef_params(input, test_ctx(input), false, false, true, 3).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 18: lr_params
    // -----------------------------------------------------------------------

    #[test]
    fn lr_params_lossless_early_return() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) =
            lr_params(input, test_ctx(input), true, false, true, false, 3, (1, 1)).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn lr_params_intrabc_early_return() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) =
            lr_params(input, test_ctx(input), false, true, true, false, 3, (1, 1)).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn lr_params_disabled_early_return() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = lr_params(
            input,
            test_ctx(input),
            false,
            false,
            false,
            false,
            3,
            (1, 1),
        )
        .unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn lr_params_all_restore_none() {
        let mut bits = BitBuilder::default();
        // 3 planes, all lr_type=0 (RESTORE_NONE)
        bits.push_bits(0, 2);
        bits.push_bits(0, 2);
        bits.push_bits(0, 2);
        // uses_lr=false → no shift bits
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) =
            lr_params(input, test_ctx(input), false, false, true, false, 3, (1, 1)).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn lr_params_luma_lr_64x64_sb() {
        let mut bits = BitBuilder::default();
        // plane 0: lr_type=1 (non-zero), planes 1,2: lr_type=0
        bits.push_bits(1, 2);
        bits.push_bits(0, 2);
        bits.push_bits(0, 2);
        // uses_lr=true, use_128x128=false
        bits.push_bool(true); // lr_unit_shift = true
        bits.push_bool(false); // lr_unit_extra_shift
        // subsampling=(1,1) but uses_chroma_lr=false → no lr_uv_shift
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) =
            lr_params(input, test_ctx(input), false, false, true, false, 3, (1, 1)).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn lr_params_chroma_lr_128x128_sb_subsample() {
        let mut bits = BitBuilder::default();
        // plane 0: none, plane 1: lr_type=1, plane 2: none
        bits.push_bits(0, 2);
        bits.push_bits(1, 2);
        bits.push_bits(0, 2);
        // uses_lr=true, use_128x128=true
        bits.push_bool(false); // lr_unit_shift
        // subsampling=(1,1) && uses_chroma_lr=true → reads lr_uv_shift
        bits.push_bool(true); // lr_uv_shift
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) =
            lr_params(input, test_ctx(input), false, false, true, true, 3, (1, 1)).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn lr_params_no_subsampling_skips_uv_shift() {
        let mut bits = BitBuilder::default();
        bits.push_bits(0, 2);
        bits.push_bits(1, 2); // chroma lr
        bits.push_bits(0, 2);
        // uses_lr=true, use_128x128=true
        bits.push_bool(true); // lr_unit_shift
        // subsampling=(0,0) → no lr_uv_shift even though uses_chroma_lr=true
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) =
            lr_params(input, test_ctx(input), false, false, true, true, 3, (0, 0)).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 19: read_tx_mode
    // -----------------------------------------------------------------------

    #[test]
    fn read_tx_mode_lossless_reads_nothing() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = read_tx_mode(input, test_ctx(input), true).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn read_tx_mode_not_lossless_reads_one_bit() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // tx_mode_select
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = read_tx_mode(input, test_ctx(input), false).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 20: frame_reference_mode
    // -----------------------------------------------------------------------

    #[test]
    fn frame_reference_mode_intra_returns_false() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, val) = frame_reference_mode(input, test_ctx(input), true).unwrap();
        assert!(!val);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn frame_reference_mode_non_intra_true() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true);
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, val) = frame_reference_mode(input, test_ctx(input), false).unwrap();
        assert!(val);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn frame_reference_mode_non_intra_false() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false);
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, val) = frame_reference_mode(input, test_ctx(input), false).unwrap();
        assert!(!val);
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 21: skip_mode_params
    // -----------------------------------------------------------------------

    #[test]
    fn skip_mode_params_intra_reads_nothing() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let ref_order = [0u64; 8];
        let ref_idx = [0usize; 7];
        let input: BitInput = (&data, 0);
        let (rem, _) = skip_mode_params(
            input,
            test_ctx(input),
            true,
            true,
            4,
            10,
            &ref_order,
            &ref_idx,
        )
        .unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn skip_mode_params_ref_select_false() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let ref_order = [0u64; 8];
        let ref_idx = [0usize; 7];
        let input: BitInput = (&data, 0);
        let (rem, _) = skip_mode_params(
            input,
            test_ctx(input),
            false,
            false,
            4,
            10,
            &ref_order,
            &ref_idx,
        )
        .unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn skip_mode_params_order_hint_bits_zero() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let ref_order = [0u64; 8];
        let ref_idx = [0usize; 7];
        let input: BitInput = (&data, 0);
        let (rem, _) = skip_mode_params(
            input,
            test_ctx(input),
            false,
            true,
            0,
            10,
            &ref_order,
            &ref_idx,
        )
        .unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn skip_mode_params_forward_and_backward_refs() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // skip_mode_present
        let (data, consumed) = with_trailer(bits);
        // order_hint=10, ref 0 has hint 5 (forward), ref 1 has hint 12 (backward)
        let mut ref_order = [0u64; 8];
        ref_order[0] = 5; // forward (dist < 0)
        ref_order[1] = 12; // backward (dist > 0)
        let ref_idx = [0, 1, 0, 0, 0, 0, 0];
        let input: BitInput = (&data, 0);
        let (rem, _) = skip_mode_params(
            input,
            test_ctx(input),
            false,
            true,
            4,
            10,
            &ref_order,
            &ref_idx,
        )
        .unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn skip_mode_params_two_forward_refs() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // skip_mode_present
        let (data, consumed) = with_trailer(bits);
        // order_hint=10, ref 0 has hint 5 (forward), ref 1 has hint 3 (also forward, further back)
        let mut ref_order = [0u64; 8];
        ref_order[0] = 5;
        ref_order[1] = 3;
        let ref_idx = [0, 1, 0, 0, 0, 0, 0];
        let input: BitInput = (&data, 0);
        let (rem, _) = skip_mode_params(
            input,
            test_ctx(input),
            false,
            true,
            4,
            10,
            &ref_order,
            &ref_idx,
        )
        .unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn skip_mode_params_one_forward_no_backward() {
        // One forward ref, but no second forward and no backward → skip_mode_allowed=false
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        // All refs point to same hint that's forward
        let mut ref_order = [0u64; 8];
        ref_order[0] = 5;
        // All ref_idx point to slot 0
        let ref_idx = [0, 0, 0, 0, 0, 0, 0];
        let input: BitInput = (&data, 0);
        let (rem, _) = skip_mode_params(
            input,
            test_ctx(input),
            false,
            true,
            4,
            10,
            &ref_order,
            &ref_idx,
        )
        .unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 22: get_relative_dist
    // -----------------------------------------------------------------------

    #[test]
    fn get_relative_dist_zero_order_hint_bits() {
        assert_eq!(get_relative_dist(5, 3, 0), 0);
    }

    #[test]
    fn get_relative_dist_positive_diff() {
        assert_eq!(get_relative_dist(5, 3, 4), 2);
    }

    #[test]
    fn get_relative_dist_negative_diff() {
        assert_eq!(get_relative_dist(3, 5, 4), -2);
    }

    #[test]
    fn get_relative_dist_wrap_positive() {
        // a=15, b=1, bits=4 → diff=14, m=8
        // (14 & 7) - (14 & 8) = 6 - 8 = -2
        assert_eq!(get_relative_dist(15, 1, 4), -2);
    }

    #[test]
    fn get_relative_dist_wrap_negative() {
        // a=1, b=15, bits=4 → diff=-14, m=8
        // (-14 & 7) - (-14 & 8) = 2 - 0 = 2
        assert_eq!(get_relative_dist(1, 15, 4), 2);
    }

    #[test]
    fn get_relative_dist_same_values() {
        assert_eq!(get_relative_dist(5, 5, 4), 0);
    }

    // -----------------------------------------------------------------------
    // Group 23: initialize_prev_gm_params
    // -----------------------------------------------------------------------

    #[test]
    fn initialize_prev_gm_params_correct_dimensions() {
        let params = initialize_prev_gm_params();
        assert_eq!(params.len(), 8);
        for row in &params {
            assert_eq!(row.len(), 6);
        }
    }

    #[test]
    fn initialize_prev_gm_params_identity_values() {
        let params = initialize_prev_gm_params();
        let identity_val = 1i32 << WARPEDMODEL_PREC_BITS;
        for row in &params {
            assert_eq!(row[0], 0);
            assert_eq!(row[1], 0);
            assert_eq!(row[2], identity_val);
            assert_eq!(row[3], 0);
            assert_eq!(row[4], 0);
            assert_eq!(row[5], identity_val);
        }
    }

    // -----------------------------------------------------------------------
    // Group 24: inverse_recenter
    // -----------------------------------------------------------------------

    #[test]
    fn inverse_recenter_v_gt_2r() {
        assert_eq!(inverse_recenter(3, 7), 7);
    }

    #[test]
    fn inverse_recenter_v_odd() {
        // r=10, v=5 (odd) → r - (v+1)/2 = 10 - 3 = 7
        assert_eq!(inverse_recenter(10, 5), 7);
    }

    #[test]
    fn inverse_recenter_v_even() {
        // r=10, v=4 (even) → r + v/2 = 10 + 2 = 12
        assert_eq!(inverse_recenter(10, 4), 12);
    }

    #[test]
    fn inverse_recenter_v_zero() {
        // r=5, v=0 (even) → r + 0 = 5
        assert_eq!(inverse_recenter(5, 0), 5);
    }

    #[test]
    fn inverse_recenter_v_one() {
        // r=5, v=1 (odd) → r - 1 = 4
        assert_eq!(inverse_recenter(5, 1), 4);
    }

    #[test]
    fn inverse_recenter_r_zero() {
        // r=0, v=3 → v > 2*0 → returns v
        assert_eq!(inverse_recenter(0, 3), 3);
    }

    // -----------------------------------------------------------------------
    // Group 25: decode_subexp
    // -----------------------------------------------------------------------

    #[test]
    fn decode_subexp_final_bits_small_num_syms() {
        // num_syms=10: first iteration, i=0, k=3, b2=3, a=8
        // num_syms(10) <= mk(0) + 3*a(24)? Yes → ns(10-0=10)
        // Encode value 5 as ns(10)
        let mut bits = BitBuilder::default();
        bits.push_ns(5, 10);
        let (data, consumed) = with_trailer(bits);
        let (rem, val) = decode_subexp((&data, 0), 10).unwrap();
        assert_eq!(val, 5);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn decode_subexp_subexp_bits_first_iter() {
        // num_syms=100: i=0, k=3, b2=3, a=8
        // num_syms(100) <= mk(0) + 3*8(24)? No
        // Read subexp_more_bits=false, then read b2=3 bits
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // more = false
        bits.push_bits(5, 3); // subexp_bits = 5; result = 5 + mk(0) = 5
        let (data, consumed) = with_trailer(bits);
        let (rem, val) = decode_subexp((&data, 0), 100).unwrap();
        assert_eq!(val, 5);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn decode_subexp_increment_then_final_ns() {
        // num_syms=30: i=0, k=3, b2=3, a=8
        // 30 <= 0+24? No → read more=true → i=1, mk=8
        // i=1, k=3, b2=3+1-1=3, a=8
        // 30 <= 8+24? Yes → ns(30-8=22)
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // more = true
        bits.push_ns(4, 22); // ns(22) = 4; result = 4 + mk(8) = 12
        let (data, consumed) = with_trailer(bits);
        let (rem, val) = decode_subexp((&data, 0), 30).unwrap();
        assert_eq!(val, 12);
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 26: decode_signed/unsigned_subexp_with_ref
    // -----------------------------------------------------------------------

    #[test]
    fn decode_signed_subexp_with_ref_basic() {
        // low=-10, high=11, r=0
        // Calls unsigned with mx=high-low=21, r_unsigned=r-low=0-(-10)=10
        // r_unsigned<<1 = 20 <= mx(21): uses inverse_recenter path
        // We encode a value that decodes to something in [-10, 11)
        let mut bits = BitBuilder::default();
        // decode_subexp(21): num_syms=21, i=0, k=3, b2=3, a=8, 21 <= 0+24 → ns(21)
        bits.push_ns(0, 21); // v=0 → inverse_recenter(10, 0) = 10 → signed = 10 + (-10) = 0
        let (data, consumed) = with_trailer(bits);
        let (rem, val) = decode_signed_subexp_with_ref((&data, 0), -10, 11, 0).unwrap();
        assert_eq!(val, 0);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn decode_unsigned_subexp_with_ref_r_small() {
        // mx=20, r=3: r<<1(6) <= mx(20) → inverse_recenter(r, v)
        let mut bits = BitBuilder::default();
        // decode_subexp(20): ns(20), encode v=0 → inverse_recenter(3, 0)=3
        bits.push_ns(0, 20);
        let (data, consumed) = with_trailer(bits);
        let (rem, val) = decode_unsigned_subexp_with_ref((&data, 0), 20, 3).unwrap();
        assert_eq!(val, 3);
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn decode_unsigned_subexp_with_ref_r_large() {
        // mx=20, r=15: r<<1(30) > mx(20) → complement path
        // mx-1-r = 19-15=4, inverse_recenter(4, v), result = mx-1-that
        let mut bits = BitBuilder::default();
        // decode_subexp(20): ns(20), encode v=0 → inverse_recenter(4, 0) = 4
        // result = 19 - 4 = 15
        bits.push_ns(0, 20);
        let (data, consumed) = with_trailer(bits);
        let (rem, val) = decode_unsigned_subexp_with_ref((&data, 0), 20, 15).unwrap();
        assert_eq!(val, 15);
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 27: global_motion_params
    // -----------------------------------------------------------------------

    #[test]
    fn global_motion_params_intra_early_return() {
        let bits = BitBuilder::default();
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = global_motion_params(input, test_ctx(input), true, true).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn global_motion_params_all_identity() {
        let mut bits = BitBuilder::default();
        // 7 refs (Last..Altref), each is_global=false
        for _ in 0..7 {
            bits.push_bool(false);
        }
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = global_motion_params(input, test_ctx(input), false, true).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn global_motion_params_single_translation() {
        let mut bits = BitBuilder::default();
        // Ref 0 (Last): translation
        bits.push_bool(true); // is_global
        bits.push_bool(false); // not rotzoom
        bits.push_bool(true); // is_translation
        // Translation reads params 0 and 1 via read_global_param.
        // Each calls decode_signed_subexp_with_ref → decode_unsigned_subexp_with_ref
        //   → decode_subexp(num_syms=1025).
        // decode_subexp(1025): i=0, k=3, b2=3, a=8; 1025 > 0+24
        //   → reads more_bit. Encode more=false, then 3 bits for value 0.
        for _ in 0..2 {
            bits.push_bool(false); // subexp_more = false
            bits.push_bits(0, 3); // subexp_bits = 0
        }
        // Refs 1-6: identity
        for _ in 1..7 {
            bits.push_bool(false);
        }
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = global_motion_params(input, test_ctx(input), false, true).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn global_motion_params_rotzoom() {
        let mut bits = BitBuilder::default();
        // Ref 0 (Last): rotzoom
        bits.push_bool(true); // is_global
        bits.push_bool(true); // is_rot_zoom
        // Rotzoom reads params 2,3 (alpha) then 0,1 (translation).
        // Each param goes through decode_subexp(8193):
        //   i=0, k=3, b2=3, a=8; 8193 > 0+24
        //   → reads more_bit. Encode more=false + 3-bit value.
        for _ in 0..4 {
            bits.push_bool(false); // subexp_more = false
            bits.push_bits(0, 3); // subexp_bits = 0
        }
        // Refs 1-6: identity
        for _ in 1..7 {
            bits.push_bool(false);
        }
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = global_motion_params(input, test_ctx(input), false, true).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    #[test]
    fn global_motion_params_affine() {
        let mut bits = BitBuilder::default();
        // Ref 0: affine
        bits.push_bool(true); // is_global
        bits.push_bool(false); // not rotzoom
        bits.push_bool(false); // not translation → affine
        // Affine reads params 2,3,4,5 then 0,1 (6 params total).
        // Each goes through decode_subexp(8193): more=false + 3-bit value.
        for _ in 0..6 {
            bits.push_bool(false); // subexp_more = false
            bits.push_bits(0, 3); // subexp_bits = 0
        }
        // Refs 1-6: identity
        for _ in 1..7 {
            bits.push_bool(false);
        }
        let (data, consumed) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (rem, _) = global_motion_params(input, test_ctx(input), false, true).unwrap();
        assert_remaining_position(rem, &data, consumed);
    }

    // -----------------------------------------------------------------------
    // Group 28: get_qindex
    // -----------------------------------------------------------------------

    #[test]
    fn get_qindex_no_seg_returns_base() {
        assert_eq!(get_qindex(true, 0, 100, None, None), 100);
    }

    #[test]
    fn get_qindex_seg_adds_offset() {
        let mut data: super::SegmentationData = Default::default();
        data[0][0] = Some(10);
        assert_eq!(get_qindex(true, 0, 100, None, Some(&data)), 110);
    }

    #[test]
    fn get_qindex_seg_negative_clamps_zero() {
        let mut data: super::SegmentationData = Default::default();
        data[0][0] = Some(-50);
        assert_eq!(get_qindex(true, 0, 30, None, Some(&data)), 0);
    }

    #[test]
    fn get_qindex_seg_overflow_clamps_255() {
        let mut data: super::SegmentationData = Default::default();
        data[0][0] = Some(200);
        assert_eq!(get_qindex(true, 0, 200, None, Some(&data)), 255);
    }

    #[test]
    fn get_qindex_seg_with_current_q() {
        let mut data: super::SegmentationData = Default::default();
        data[0][0] = Some(10);
        assert_eq!(get_qindex(false, 0, 100, Some(80), Some(&data)), 90);
    }

    #[test]
    fn get_qindex_seg_ignore_delta_uses_base() {
        let mut data: super::SegmentationData = Default::default();
        data[0][0] = Some(10);
        assert_eq!(get_qindex(true, 0, 100, Some(80), Some(&data)), 110);
    }

    #[test]
    fn get_qindex_no_seg_returns_current_q() {
        assert_eq!(get_qindex(false, 0, 100, Some(80), None), 80);
    }

    #[test]
    fn get_qindex_no_seg_ignore_delta_returns_base() {
        assert_eq!(get_qindex(true, 0, 100, Some(80), None), 100);
    }

    // -----------------------------------------------------------------------
    // Group 29: seg_feature_active_idx
    // -----------------------------------------------------------------------

    #[test]
    fn seg_feature_active_idx_none_data() {
        assert!(!seg_feature_active_idx(0, 0, None));
    }

    #[test]
    fn seg_feature_active_idx_none_feature() {
        let data: super::SegmentationData = Default::default();
        assert!(!seg_feature_active_idx(0, 0, Some(&data)));
    }

    #[test]
    fn seg_feature_active_idx_some_feature() {
        let mut data: super::SegmentationData = Default::default();
        data[2][3] = Some(42);
        assert!(seg_feature_active_idx(2, 3, Some(&data)));
    }

    // ===================================================================
    // BitstreamParser method tests — infrastructure and groups A–E
    // ===================================================================

    use super::super::{
        BitstreamParser,
        grain::{FilmGrainHeader, FilmGrainParams, film_grain_params},
        obu::{ObuHeader, ObuType},
        sequence::{
            ColorConfig, ColorPrimaries, ColorRange, MatrixCoefficients, SequenceHeader,
            TransferCharacteristics,
        },
    };
    use super::{FrameHeader, NUM_REF_FRAMES, TileInfo};
    use crate::GrainTableSegment;
    use arrayvec::ArrayVec;
    use av1_grain::DEFAULT_GRAIN_SEED;

    fn grain_test_ctx(input: BitInput) -> TraceCtx {
        TraceCtx::new(input, 0)
    }

    fn make_parser<const WRITE: bool>() -> BitstreamParser<WRITE> {
        BitstreamParser {
            reader: None,
            writer: None,
            packet_out: Vec::new(),
            incoming_grain_header: None,
            parsed: false,
            size: 0,
            seen_frame_header: false,
            sequence_header: None,
            previous_frame_header: None,
            ref_frame_idx: Default::default(),
            ref_order_hint: Default::default(),
            big_ref_order_hint: Default::default(),
            big_ref_valid: Default::default(),
            big_order_hints: Default::default(),
            grain_headers: Vec::new(),
        }
    }

    fn minimal_sequence_header() -> SequenceHeader {
        SequenceHeader {
            reduced_still_picture_header: false,
            frame_id_numbers_present: false,
            additional_frame_id_len_minus_1: 0,
            delta_frame_id_len_minus_2: 0,
            film_grain_params_present: false,
            new_film_grain_state: false,
            force_screen_content_tools: 1,
            force_integer_mv: 1,
            order_hint_bits: 0,
            frame_width_bits_minus_1: 0,
            frame_height_bits_minus_1: 0,
            max_frame_width_minus_1: 7,
            max_frame_height_minus_1: 7,
            decoder_model_info: None,
            decoder_model_present_for_op: ArrayVec::new(),
            operating_points_cnt_minus_1: 0,
            operating_point_idc: ArrayVec::new(),
            cur_operating_point_idc: 0,
            timing_info: None,
            enable_ref_frame_mvs: false,
            enable_warped_motion: false,
            enable_superres: false,
            enable_cdef: false,
            enable_restoration: false,
            use_128x128_superblock: false,
            color_config: ColorConfig {
                color_primaries: ColorPrimaries::Unspecified,
                transfer_characteristics: TransferCharacteristics::Unspecified,
                matrix_coefficients: MatrixCoefficients::Unspecified,
                color_range: ColorRange::Full,
                num_planes: 1,
                separate_uv_delta_q: false,
                subsampling: (0, 0),
            },
        }
    }

    fn simple_obu_header() -> ObuHeader {
        ObuHeader {
            obu_type: ObuType::Frame,
            has_size_field: false,
            extension: None,
        }
    }

    fn minimal_grain_params() -> FilmGrainParams {
        // RATIONALE: The film_grain_params parser always pushes a default [0]
        // into ar_coeffs_cb/cr when no chroma coefficients are signaled
        // (monochrome or no cb/cr points). Pre-populate these so roundtrip
        // comparisons match.
        let mut ar_coeffs_cb = ArrayVec::new();
        ar_coeffs_cb.push(0i8);
        let mut ar_coeffs_cr = ArrayVec::new();
        ar_coeffs_cr.push(0i8);
        FilmGrainParams {
            grain_seed: 0,
            scaling_points_y: ArrayVec::new(),
            scaling_points_cb: ArrayVec::new(),
            scaling_points_cr: ArrayVec::new(),
            scaling_shift: 8,
            ar_coeff_lag: 0,
            ar_coeffs_y: ArrayVec::new(),
            ar_coeffs_cb,
            ar_coeffs_cr,
            ar_coeff_shift: 6,
            cb_mult: 0,
            cb_luma_mult: 0,
            cb_offset: 0,
            cr_mult: 0,
            cr_luma_mult: 0,
            cr_offset: 0,
            chroma_scaling_from_luma: false,
            grain_scale_shift: 0,
            overlap_flag: false,
            clip_to_restricted_range: false,
        }
    }

    /// Builds the bits for a minimal Key frame header (shown or hidden)
    /// against `minimal_sequence_header()`.
    fn build_minimal_key_frame_bits(show_frame: bool) -> BitBuilder {
        let mut bits = BitBuilder::default();
        bits.push_bool(false); // show_existing_frame = 0
        bits.push_bits(0, 2); // frame_type = Key (00)
        bits.push_bool(show_frame);
        if !show_frame {
            bits.push_bool(false); // showable_frame = 0
            bits.push_bool(false); // error_resilient_mode = 0
        }
        // Key+show → error_resilient implicit true, no bit
        bits.push_bool(false); // disable_cdf_update
        // force_screen_content_tools=1 → allow_screen_content_tools=true, no bit
        // force_integer_mv=1, not SELECT_INTEGER_MV → no bit
        // frame_id_numbers_present=false → no bit
        bits.push_bool(false); // frame_size_override_flag
        // order_hint_bits=0 → 0 bits for order_hint
        // intra → primary_ref_frame=PRIMARY_REF_NONE, no bit
        // no decoder_model_info → skip
        if !show_frame {
            bits.push_bits(0xFF, 8); // refresh_frame_flags
        }
        // Key+show → REFRESH_ALL_FRAMES implicit, no bit
        // Intra path: frame_size(no override, no superres)=no bits
        bits.push_bool(false); // render_different = 0
        // allow_screen_content_tools && width==width → allow_intrabc bit
        bits.push_bool(false); // allow_intrabc = 0
        // disable_frame_end_update_cdf: not reduced_still, not disable_cdf → read bit
        bits.push_bool(false); // disable_frame_end_update_cdf = 0
        // primary_ref=NONE → init_non_coeff_cdfs + setup_past_independence (no-ops)
        // use_ref_frame_mvs=false → skip
        bits.push_bool(true); // uniform_tile_spacing_flag = 1
        // With 8x8 frame, sb_cols=sb_rows=1 → tile_cols_log2=0, no increment loops
        // tile_cols_log2+tile_rows_log2=0 → no context_update_tile_id
        bits.push_bits(0, 8); // base_q_idx = 0
        bits.push_bool(false); // deltaq_y_dc_coded = 0
        // num_planes=1 → no chroma delta-q
        bits.push_bool(false); // using_qmatrix = 0
        bits.push_bool(false); // seg_enabled = 0
        // base_q_idx=0 → delta_q_params returns false, no bit
        // coded_lossless=true → loop_filter, cdef, lr, tx_mode all early-return
        // intra → frame_reference_mode=false, skip_mode=false, warped=false, no bits
        bits.push_bool(false); // reduced_tx_set = 0
        // intra → global_motion_params no-op
        // film_grain_params_present=false → no grain bits
        bits
    }

    // -----------------------------------------------------------------------
    // Group A: write_film_grain_disabled_bit
    // -----------------------------------------------------------------------

    #[test]
    fn write_grain_disabled_no_extra_bits() {
        let mut parser = make_parser::<true>();
        parser.sequence_header = Some(minimal_sequence_header());
        parser.write_film_grain_disabled_bit(0x00, 0);
        assert_eq!(parser.packet_out, vec![0x00]);
    }

    #[test]
    fn write_grain_disabled_3_extra_bits() {
        let mut parser = make_parser::<true>();
        parser.sequence_header = Some(minimal_sequence_header());
        // extra_byte=0b1101_0000, 3 bits → 1,1,0 then apply_grain=0
        parser.write_film_grain_disabled_bit(0b1101_0000, 3);
        // Result: bits 1,1,0,0 → 0b1100_0000 = 0xC0
        assert_eq!(parser.packet_out, vec![0xC0]);
    }

    #[test]
    fn write_grain_disabled_7_extra_bits() {
        let mut parser = make_parser::<true>();
        parser.sequence_header = Some(minimal_sequence_header());
        // extra_byte=0b1111_1110, 7 bits → 1,1,1,1,1,1,1 then apply_grain=0
        parser.write_film_grain_disabled_bit(0b1111_1110, 7);
        // Result: bits 1,1,1,1,1,1,1,0 → 0b1111_1110 = 0xFE
        assert_eq!(parser.packet_out, vec![0xFE]);
    }

    #[test]
    fn write_grain_disabled_1_extra_bit() {
        let mut parser = make_parser::<true>();
        parser.sequence_header = Some(minimal_sequence_header());
        // extra_byte=0b1000_0000, 1 bit → 1 then apply_grain=0
        parser.write_film_grain_disabled_bit(0b1000_0000, 1);
        // Result: bits 1,0 → 0b1000_0000 = 0x80
        assert_eq!(parser.packet_out, vec![0x80]);
    }

    // -----------------------------------------------------------------------
    // Group B: write_film_grain_bits roundtrip tests
    // -----------------------------------------------------------------------

    #[test]
    fn write_grain_roundtrip_monochrome_key_minimal() {
        let mut parser = make_parser::<true>();
        parser.sequence_header = Some(minimal_sequence_header());
        let mut params = minimal_grain_params();
        params.grain_seed = 0xABCD;
        let segment = GrainTableSegment {
            start_time: 0,
            end_time: 0,
            grain_params: params.clone(),
        };
        let result = parser.write_film_grain_bits(0, 0, &segment, FrameType::Key);
        assert!(matches!(result, FilmGrainHeader::UpdateGrain(_)));

        // Parse the written output as grain params
        let data = &parser.packet_out;
        let grain_input: BitInput = (data.as_slice(), 0);
        let (_, parsed) = film_grain_params(
            grain_input,
            grain_test_ctx(grain_input),
            true,
            FrameType::Key,
            true,
            (0, 0),
        )
        .unwrap();
        if let FilmGrainHeader::UpdateGrain(parsed_params) = parsed {
            assert_eq!(parsed_params, params);
        } else {
            panic!("expected UpdateGrain, got {parsed:?}");
        }
    }

    #[test]
    fn write_grain_roundtrip_inter_adds_update_grain() {
        let mut parser = make_parser::<true>();
        parser.sequence_header = Some(minimal_sequence_header());
        let mut params = minimal_grain_params();
        params.grain_seed = 0x1234;
        let segment = GrainTableSegment {
            start_time: 0,
            end_time: 0,
            grain_params: params.clone(),
        };
        let result = parser.write_film_grain_bits(0, 0, &segment, FrameType::Inter);
        assert!(matches!(result, FilmGrainHeader::UpdateGrain(_)));

        let data = &parser.packet_out;
        let grain_input: BitInput = (data.as_slice(), 0);
        let (_, parsed) = film_grain_params(
            grain_input,
            grain_test_ctx(grain_input),
            true,
            FrameType::Inter,
            true,
            (0, 0),
        )
        .unwrap();
        if let FilmGrainHeader::UpdateGrain(parsed_params) = parsed {
            assert_eq!(parsed_params, params);
        } else {
            panic!("expected UpdateGrain, got {parsed:?}");
        }
    }

    #[test]
    fn write_grain_roundtrip_y_scaling_points() {
        let mut parser = make_parser::<true>();
        parser.sequence_header = Some(minimal_sequence_header());
        let mut params = minimal_grain_params();
        params.grain_seed = 0x5678;
        params.scaling_points_y.push([10, 20]);
        params.scaling_points_y.push([30, 40]);
        let segment = GrainTableSegment {
            start_time: 0,
            end_time: 0,
            grain_params: params.clone(),
        };
        let result = parser.write_film_grain_bits(0, 0, &segment, FrameType::Key);
        assert!(matches!(result, FilmGrainHeader::UpdateGrain(_)));

        let data = &parser.packet_out;
        let grain_input: BitInput = (data.as_slice(), 0);
        let (_, parsed) = film_grain_params(
            grain_input,
            grain_test_ctx(grain_input),
            true,
            FrameType::Key,
            true,
            (0, 0),
        )
        .unwrap();
        if let FilmGrainHeader::UpdateGrain(parsed_params) = parsed {
            assert_eq!(parsed_params.scaling_points_y.len(), 2);
            assert_eq!(parsed_params.scaling_points_y[0], [10, 20]);
            assert_eq!(parsed_params.scaling_points_y[1], [30, 40]);
            assert_eq!(parsed_params, params);
        } else {
            panic!("expected UpdateGrain, got {parsed:?}");
        }
    }

    #[test]
    fn write_grain_roundtrip_non_mono_chroma_points() {
        let mut parser = make_parser::<true>();
        let mut seq = minimal_sequence_header();
        seq.color_config.num_planes = 3;
        seq.color_config.subsampling = (0, 0);
        parser.sequence_header = Some(seq);
        let mut params = minimal_grain_params();
        params.grain_seed = 0x9ABC;
        params.scaling_points_y.push([50, 60]);
        params.scaling_points_cb.push([70, 80]);
        params.scaling_points_cr.push([90, 100]);
        params.cb_mult = 128;
        params.cb_luma_mult = 192;
        params.cb_offset = 256;
        params.cr_mult = 64;
        params.cr_luma_mult = 32;
        params.cr_offset = 128;
        let segment = GrainTableSegment {
            start_time: 0,
            end_time: 0,
            grain_params: params.clone(),
        };
        let result = parser.write_film_grain_bits(0, 0, &segment, FrameType::Key);
        assert!(matches!(result, FilmGrainHeader::UpdateGrain(_)));

        let data = &parser.packet_out;
        let grain_input: BitInput = (data.as_slice(), 0);
        let (_, parsed) = film_grain_params(
            grain_input,
            grain_test_ctx(grain_input),
            true,
            FrameType::Key,
            false,
            (0, 0),
        )
        .unwrap();
        if let FilmGrainHeader::UpdateGrain(parsed_params) = parsed {
            assert_eq!(parsed_params.scaling_points_cb[0], [70, 80]);
            assert_eq!(parsed_params.scaling_points_cr[0], [90, 100]);
            assert_eq!(parsed_params.cb_mult, 128);
            assert_eq!(parsed_params.cr_offset, 128);
            assert_eq!(parsed_params, params);
        } else {
            panic!("expected UpdateGrain, got {parsed:?}");
        }
    }

    #[test]
    fn write_grain_roundtrip_chroma_scaling_from_luma() {
        let mut parser = make_parser::<true>();
        let mut seq = minimal_sequence_header();
        seq.color_config.num_planes = 3;
        seq.color_config.subsampling = (0, 0);
        parser.sequence_header = Some(seq);
        let mut params = minimal_grain_params();
        params.grain_seed = 0xDEF0;
        params.scaling_points_y.push([15, 25]);
        params.chroma_scaling_from_luma = true;
        params.ar_coeff_lag = 1;
        // ar_coeff_lag=1 → num_pos_luma = 2*1*2 = 4
        // num_y_points>0 → num_pos_chroma = 4+1 = 5
        for _ in 0..4 {
            params.ar_coeffs_y.push(0);
        }
        // chroma_scaling_from_luma → need ar_coeffs_cb and ar_coeffs_cr of len num_pos_chroma=5
        params.ar_coeffs_cb.clear();
        params.ar_coeffs_cr.clear();
        for _ in 0..5 {
            params.ar_coeffs_cb.push(0);
            params.ar_coeffs_cr.push(0);
        }
        let segment = GrainTableSegment {
            start_time: 0,
            end_time: 0,
            grain_params: params.clone(),
        };
        let result = parser.write_film_grain_bits(0, 0, &segment, FrameType::Key);
        assert!(matches!(result, FilmGrainHeader::UpdateGrain(_)));

        let data = &parser.packet_out;
        let grain_input: BitInput = (data.as_slice(), 0);
        let (_, parsed) = film_grain_params(
            grain_input,
            grain_test_ctx(grain_input),
            true,
            FrameType::Key,
            false,
            (0, 0),
        )
        .unwrap();
        if let FilmGrainHeader::UpdateGrain(parsed_params) = parsed {
            assert!(parsed_params.chroma_scaling_from_luma);
            assert!(parsed_params.scaling_points_cb.is_empty());
            assert!(parsed_params.scaling_points_cr.is_empty());
            assert_eq!(parsed_params, params);
        } else {
            panic!("expected UpdateGrain, got {parsed:?}");
        }
    }

    #[test]
    fn write_grain_extra_bits_prefix_preserved() {
        let mut parser = make_parser::<true>();
        parser.sequence_header = Some(minimal_sequence_header());
        let mut params = minimal_grain_params();
        params.grain_seed = 0;
        let segment = GrainTableSegment {
            start_time: 0,
            end_time: 0,
            grain_params: params,
        };
        // extra_byte=0b11100000, extra_bits_used=3 → prefix bits: 1,1,1
        parser.write_film_grain_bits(0b1110_0000, 3, &segment, FrameType::Key);
        // First 3 bits should be 111, bit 3 should be apply_grain=1
        let first_byte = parser.packet_out[0];
        assert_eq!(first_byte >> 5, 0b111); // top 3 bits
        assert_eq!((first_byte >> 4) & 1, 1); // apply_grain=1 at bit 3
    }

    // -----------------------------------------------------------------------
    // Group C: parse_frame_header
    // -----------------------------------------------------------------------

    #[test]
    fn parse_frame_header_seen_flag_short_circuits() {
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        parser.seen_frame_header = true;
        let input: &[u8] = &[0xAB, 0xCD];
        let (remaining, result) = parser
            .parse_frame_header(input, simple_obu_header(), 0, 0, false)
            .unwrap();
        assert!(result.is_none());
        assert_eq!(
            remaining.len(),
            input.len(),
            "input pointer should be unchanged"
        );
    }

    #[test]
    fn parse_frame_header_show_existing_frame() {
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        parser.previous_frame_header = Some(FrameHeader {
            show_frame: true,
            show_existing_frame: false,
            film_grain_params: FilmGrainHeader::Disable,
            tile_info: TileInfo {
                tile_cols: 2,
                tile_rows: 3,
                tile_cols_log2: 1,
                tile_rows_log2: 2,
            },
        });
        // show_existing_frame=1 (1 bit), frame_to_show_map_idx=000 (3 bits)
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // show_existing_frame
        bits.push_bits(0, 3); // frame_to_show_map_idx
        let (data, _) = with_trailer(bits);
        let (_, result) = parser
            .parse_frame_header(&data, simple_obu_header(), 0, 0, false)
            .unwrap();
        let header = result.expect("shown existing frame should return Some");
        assert!(header.show_existing_frame);
        assert_eq!(header.film_grain_params, FilmGrainHeader::CopyRefFrame);
        // seen_frame_header should be cleared for show_existing_frame
        assert!(!parser.seen_frame_header);
    }

    #[test]
    fn parse_frame_header_shown_key_frame() {
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        let bits = build_minimal_key_frame_bits(true);
        let (data, _) = with_trailer(bits);
        let (_, result) = parser
            .parse_frame_header(&data, simple_obu_header(), 0, 0, false)
            .unwrap();
        let header = result.expect("shown Key frame should return Some");
        assert!(header.show_frame);
        assert!(!header.show_existing_frame);
        assert!(parser.seen_frame_header);
    }

    #[test]
    fn parse_frame_header_hidden_frame_returns_none() {
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        let bits = build_minimal_key_frame_bits(false);
        let (data, _) = with_trailer(bits);
        let (_, result) = parser
            .parse_frame_header(&data, simple_obu_header(), 0, 0, false)
            .unwrap();
        assert!(result.is_none(), "hidden frame should return None");
        assert!(parser.seen_frame_header);
    }

    #[test]
    fn parse_frame_header_show_existing_carries_tile_info() {
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        let expected_tile_info = TileInfo {
            tile_cols: 4,
            tile_rows: 5,
            tile_cols_log2: 2,
            tile_rows_log2: 3,
        };
        parser.previous_frame_header = Some(FrameHeader {
            show_frame: true,
            show_existing_frame: false,
            film_grain_params: FilmGrainHeader::Disable,
            tile_info: expected_tile_info,
        });
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // show_existing_frame
        bits.push_bits(0, 3); // frame_to_show_map_idx
        let (data, _) = with_trailer(bits);
        let (_, result) = parser
            .parse_frame_header(&data, simple_obu_header(), 0, 0, false)
            .unwrap();
        let header = result.unwrap();
        assert_eq!(header.tile_info.tile_cols, expected_tile_info.tile_cols);
        assert_eq!(header.tile_info.tile_rows, expected_tile_info.tile_rows);
    }

    // -----------------------------------------------------------------------
    // Group D: uncompressed_header specifics
    // -----------------------------------------------------------------------

    #[test]
    fn uncompressed_header_key_show_resets_refs() {
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        // Pre-populate ref state
        for i in 0..NUM_REF_FRAMES {
            parser.big_ref_valid[i] = true;
            parser.big_ref_order_hint[i] = 99;
        }
        let bits = build_minimal_key_frame_bits(true);
        let (data, _) = with_trailer(bits);
        let _ = parser
            .parse_frame_header(&data, simple_obu_header(), 0, 0, false)
            .unwrap();
        // Key+show clears refs first, then refreshes all (order_hint=0, valid=true)
        for i in 0..NUM_REF_FRAMES {
            assert!(
                parser.big_ref_valid[i],
                "ref {i} should be valid after refresh"
            );
            assert_eq!(
                parser.big_ref_order_hint[i], 0,
                "ref {i} order_hint should be 0"
            );
        }
    }

    #[test]
    fn uncompressed_header_hidden_key_does_not_reset_refs() {
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        // Pre-populate ref state
        for i in 0..NUM_REF_FRAMES {
            parser.big_ref_valid[i] = true;
            parser.big_ref_order_hint[i] = 99;
        }
        let bits = build_minimal_key_frame_bits(false);
        let (data, _) = with_trailer(bits);
        let _ = parser
            .parse_frame_header(&data, simple_obu_header(), 0, 0, false)
            .unwrap();
        // Hidden Key frame: no clear step (Key+show_frame required), but
        // refresh_frame_flags=0xFF refreshes all → valid=true, order_hint=0
        for i in 0..NUM_REF_FRAMES {
            assert!(parser.big_ref_valid[i]);
            assert_eq!(
                parser.big_ref_order_hint[i], 0,
                "ref {i} should be refreshed to order_hint=0"
            );
        }
    }

    #[test]
    fn uncompressed_header_write_copies_prefix_to_packet_out() {
        let mut parser = make_parser::<true>();
        parser.sequence_header = Some(minimal_sequence_header());
        let bits = build_minimal_key_frame_bits(true);
        let (data, consumed) = with_trailer(bits);
        let _ = parser
            .parse_frame_header(&data, simple_obu_header(), 0, 0, false)
            .unwrap();
        // WRITE mode should copy the header bytes to packet_out.
        // film_grain_params_present=false and new_film_grain_state=false,
        // so the writer flushes partial byte with unused bits zeroed.
        let expected_bytes = consumed.div_ceil(8);
        assert_eq!(
            parser.packet_out.len(),
            expected_bytes,
            "packet_out should contain header bytes (consumed {consumed} bits = {expected_bytes} bytes)"
        );
    }

    #[test]
    fn uncompressed_header_write_show_existing_copies_bytes() {
        let mut parser = make_parser::<true>();
        parser.sequence_header = Some(minimal_sequence_header());
        parser.previous_frame_header = Some(FrameHeader {
            show_frame: true,
            show_existing_frame: false,
            film_grain_params: FilmGrainHeader::Disable,
            tile_info: TileInfo {
                tile_cols: 1,
                tile_rows: 1,
                tile_cols_log2: 0,
                tile_rows_log2: 0,
            },
        });
        let mut bits = BitBuilder::default();
        bits.push_bool(true); // show_existing_frame
        bits.push_bits(0, 3); // frame_to_show_map_idx
        let (data, _) = with_trailer(bits);
        let _ = parser
            .parse_frame_header(&data, simple_obu_header(), 0, 0, false)
            .unwrap();
        // 4 bits → 1 byte written to packet_out
        assert_eq!(parser.packet_out.len(), 1);
    }

    #[test]
    fn uncompressed_header_write_injects_grain_from_matching_segment() {
        let mut parser = make_parser::<true>();
        let mut seq = minimal_sequence_header();
        seq.film_grain_params_present = true;
        seq.new_film_grain_state = true;
        parser.sequence_header = Some(seq);
        let mut grain = minimal_grain_params();
        grain.grain_seed = 100;
        parser.incoming_grain_header = Some(vec![GrainTableSegment {
            start_time: 0,
            end_time: 1000,
            grain_params: grain,
        }]);
        // Build key frame bits + 1 bit for apply_grain=false (original stream)
        let mut bits = build_minimal_key_frame_bits(true);
        bits.push_bool(false); // apply_grain = false in original stream
        let (data, _) = with_trailer(bits);
        let (_, result) = parser
            .parse_frame_header(&data, simple_obu_header(), 500, 0, false)
            .unwrap();
        let header = result.unwrap();
        match &header.film_grain_params {
            FilmGrainHeader::UpdateGrain(params) => {
                assert_eq!(
                    params.grain_seed,
                    100u16.wrapping_add(DEFAULT_GRAIN_SEED),
                    "grain seed should be original + DEFAULT_GRAIN_SEED"
                );
            }
            other => panic!("expected UpdateGrain, got {other:?}"),
        }
    }

    #[test]
    fn uncompressed_header_write_disables_grain_no_matching_segment() {
        let mut parser = make_parser::<true>();
        let mut seq = minimal_sequence_header();
        seq.film_grain_params_present = true;
        seq.new_film_grain_state = true;
        parser.sequence_header = Some(seq);
        let mut grain = minimal_grain_params();
        grain.grain_seed = 100;
        // Segment time range doesn't cover packet_ts=5000
        parser.incoming_grain_header = Some(vec![GrainTableSegment {
            start_time: 0,
            end_time: 1000,
            grain_params: grain,
        }]);
        let mut bits = build_minimal_key_frame_bits(true);
        bits.push_bool(false); // apply_grain = false
        let (data, _) = with_trailer(bits);
        let (_, result) = parser
            .parse_frame_header(&data, simple_obu_header(), 5000, 0, false)
            .unwrap();
        let header = result.unwrap();
        assert_eq!(header.film_grain_params, FilmGrainHeader::Disable);
    }

    #[test]
    fn uncompressed_header_write_disables_grain_no_incoming_header() {
        let mut parser = make_parser::<true>();
        let mut seq = minimal_sequence_header();
        seq.film_grain_params_present = true;
        seq.new_film_grain_state = true;
        parser.sequence_header = Some(seq);
        parser.incoming_grain_header = None;
        let mut bits = build_minimal_key_frame_bits(true);
        bits.push_bool(false); // apply_grain = false
        let (data, _) = with_trailer(bits);
        let (_, result) = parser
            .parse_frame_header(&data, simple_obu_header(), 0, 0, false)
            .unwrap();
        let header = result.unwrap();
        assert_eq!(header.film_grain_params, FilmGrainHeader::Disable);
    }

    // -----------------------------------------------------------------------
    // Group E: parse_frame_obu
    // -----------------------------------------------------------------------

    #[test]
    fn parse_frame_obu_shown_key_returns_some() {
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        let bits = build_minimal_key_frame_bits(true);
        let mut data = bits.into_bytes();
        data.extend_from_slice(&[0xAA, 0xBB]); // tile payload
        parser.size = data.len();
        let (remaining, result) = parser
            .parse_frame_obu(&data, simple_obu_header(), 0, 0)
            .unwrap();
        let header = result.expect("shown Key frame should return Some");
        assert!(header.show_frame);
        assert!(remaining.is_empty());
    }

    #[test]
    fn parse_frame_obu_seen_header_uses_previous() {
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        parser.seen_frame_header = true;
        parser.previous_frame_header = Some(FrameHeader {
            show_frame: true,
            show_existing_frame: false,
            film_grain_params: FilmGrainHeader::Disable,
            tile_info: TileInfo {
                tile_cols: 1,
                tile_rows: 1,
                tile_cols_log2: 0,
                tile_rows_log2: 0,
            },
        });
        let data = vec![0xAA, 0xBB]; // just tile payload
        parser.size = data.len();
        let (remaining, result) = parser
            .parse_frame_obu(&data, simple_obu_header(), 0, 0)
            .unwrap();
        assert!(
            result.is_none(),
            "seen_frame_header should yield None from header"
        );
        assert!(remaining.is_empty());
        // Tile group should have cleared seen_frame_header (single tile)
        assert!(!parser.seen_frame_header);
    }

    #[test]
    fn parse_frame_obu_size_calculation() {
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        let bits = build_minimal_key_frame_bits(true);
        let header_bytes = bits.into_bytes();
        let header_len = header_bytes.len(); // 3 bytes for 22-bit header
        let tile_payload_len = 4;
        let mut data = header_bytes;
        data.extend(vec![0u8; tile_payload_len]);
        parser.size = header_len + tile_payload_len;
        // Should not panic: tile_group receives exactly tile_payload_len bytes
        let (remaining, _) = parser
            .parse_frame_obu(&data, simple_obu_header(), 0, 0)
            .unwrap();
        assert!(remaining.is_empty());
    }

    #[test]
    fn parse_frame_obu_write_accumulates_header_and_tile() {
        let mut parser = make_parser::<true>();
        parser.sequence_header = Some(minimal_sequence_header());
        let bits = build_minimal_key_frame_bits(true);
        let mut data = bits.into_bytes();
        let tile_payload = [0xCA, 0xFE];
        data.extend_from_slice(&tile_payload);
        parser.size = data.len();
        let _ = parser
            .parse_frame_obu(&data, simple_obu_header(), 0, 0)
            .unwrap();
        // packet_out should contain header + tile group bytes
        assert!(parser.packet_out.len() > tile_payload.len());
        // Tile payload should be the last bytes in packet_out
        let tail = &parser.packet_out[parser.packet_out.len() - tile_payload.len()..];
        assert_eq!(tail, &tile_payload);
    }

    // -----------------------------------------------------------------------
    // Group F: byte_alignment verification in parse_frame_obu
    // -----------------------------------------------------------------------

    #[test]
    fn parse_frame_obu_with_zero_padding_passes_alignment() {
        // build_minimal_key_frame_bits produces 22 bits → 2 zero padding bits.
        // BitBuilder::into_bytes() pads with zeros, so alignment verification succeeds.
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        let bits = build_minimal_key_frame_bits(true);
        let mut data = bits.into_bytes();
        data.extend_from_slice(&[0xAA, 0xBB]); // tile payload
        parser.size = data.len();
        let (remaining, result) = parser
            .parse_frame_obu(&data, simple_obu_header(), 0, 0)
            .expect("zero-padded frame header should pass byte_alignment");
        assert!(result.is_some());
        assert!(remaining.is_empty());
    }

    #[test]
    fn parse_frame_obu_with_non_zero_padding_errors() {
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        let bits = build_minimal_key_frame_bits(true);
        // 22 bits → occupies first 22 bits, padding is bits 22-23 (2 bits).
        // Set a non-zero padding bit to trigger alignment failure.
        let mut data = bits.into_bytes();
        // Byte 2 (bits 16-23): bits 16-21 are header, bits 22-23 are padding.
        // Set bit 22 (= bit index 6 of byte 2 = mask 0b0000_0010).
        data[2] |= 0b0000_0010;
        data.extend_from_slice(&[0xAA, 0xBB]); // tile payload
        parser.size = data.len();
        let result = parser.parse_frame_obu(&data, simple_obu_header(), 0, 0);
        assert!(
            result.is_err(),
            "non-zero padding bits should cause byte_alignment to fail"
        );
    }

    #[test]
    fn parse_frame_header_standalone_skips_alignment_verification() {
        // Standalone FrameHeader OBU uses trailing_bits() (starts with 1),
        // so verify_byte_alignment=false should skip the check.
        let mut parser = make_parser::<false>();
        parser.sequence_header = Some(minimal_sequence_header());
        let mut bits = build_minimal_key_frame_bits(true);
        // Add a trailing 1-bit (simulating trailing_bits()) followed by zeros.
        bits.push_bool(true);
        let (data, _) = with_trailer(bits);
        // With verify_byte_alignment=false, the non-zero padding bit is ignored.
        let result = parser.parse_frame_header(&data, simple_obu_header(), 0, 0, false);
        assert!(
            result.is_ok(),
            "standalone FrameHeader should skip alignment verification"
        );
    }
}
