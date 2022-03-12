use nom::{bits::complete as bit_parsers, sequence::tuple, IResult};

use crate::parser::{util::take_bool_bit, ParserContext};

impl ParserContext {
    pub(in crate::parser) fn sequence_header_obu(
        &mut self,
        input: (&[u8], usize),
    ) -> IResult<(&[u8], usize), ()> {
        let (mut input, (seq_profile, still_picture, reduced_still_picture_header)) =
            tuple((bit_parsers::take(3usize), take_bool_bit, take_bool_bit))(input)?;
        self.seq_profile = seq_profile;
        self.still_picture = still_picture;

        if reduced_still_picture_header {
            let (rem, seq_level_idx) = bit_parsers::take(5usize)(input)?;
            input = rem;
            self.seq_level_idx.push(seq_level_idx);

            self.timing_info = None;
            self.decoder_model_info = None;
            self.initial_display_delay_present_flag = false;
            self.operating_points_cnt_minus_1 = 0;
            self.operating_point_idc.push(Default::default());
            self.seq_tier.push(0);
            self.decoder_model_present_for_this_op.push(false);
            self.initial_display_delay_present_for_this_op.push(false);
        } else {
            let (rem, timing_info_present_flag) = take_bool_bit(input)?;
            input = rem;
            if timing_info_present_flag {
                let (rem, timing_info) = timing_info(input)?;
                input = rem;
                self.timing_info = Some(timing_info);

                let (rem, decoder_model_info_present_flag) = take_bool_bit(input)?;
                input = rem;
                if decoder_model_info_present_flag {
                    let (rem, decoder_model_info) = decoder_model_info(input)?;
                    input = rem;
                    self.decoder_model_info = Some(decoder_model_info);
                } else {
                    self.decoder_model_info = None;
                }
            } else {
                self.timing_info = None;
                self.decoder_model_info = None;
            }

            let (rem, initial_display_delay_present_flag) = take_bool_bit(input)?;
            input = rem;
            self.initial_display_delay_present_flag = initial_display_delay_present_flag;

            let (rem, operating_points_cnt_minus_1) = bit_parsers::take(5usize)(input)?;
            input = rem;
            self.operating_points_cnt_minus_1 = operating_points_cnt_minus_1;
            for i in 0..=self.operating_points_cnt_minus_1 {
                let (rem, operating_point_idc) = bit_parsers::take(12usize)(input)?;
                input = rem;
                self.operating_point_idc[i] = operating_point_idc;

                let (rem, seq_level_idx) = bit_parsers::take(5usize)(input)?;
                input = rem;
                self.seq_level_idx[i] = seq_level_idx;
                if self.seq_level_idx[i] > 7 {
                    let (rem, seq_tier) = take_bool_bit(input)?;
                    input = rem;
                    self.seq_tier[i] = seq_tier;
                } else {
                    self.seq_tier[i] = false;
                };

                if self.decoder_model_info.is_some() {
                    let (rem, decoder_model_present_for_this_op) = take_bool_bit(input)?;
                    input = rem;
                    self.decoder_model_present_for_this_op[i] = decoder_model_present_for_this_op;
                    if self.decoder_model_present_for_this_op[i] {
                        let (rem, operating_parameters_info) = operating_parameters_info(input, i)?;
                        input = rem;
                        self.operating_parameters_info[i] = operating_parameters_info;
                    }
                } else {
                    self.decoder_model_present_for_this_op[i] = false;
                };

                if self.initial_display_delay_present_flag {
                    let (rem, initial_display_delay_present_for_this_op) = take_bool_bit(input)?;
                    input = rem;
                    self.initial_display_delay_present_for_this_op[i] =
                        initial_display_delay_present_for_this_op;
                    if self.initial_display_delay_present_for_this_op[i] {
                        let (rem, initial_display_delay_minus_1) =
                            bit_parsers::take(4usize)(input)?;
                        input = rem;
                        self.initial_display_delay_minus_1[i] = initial_display_delay_minus_1;
                    }
                }
            }
        }

        self.operating_point = self.choose_operating_point();
        let cur_operating_point_idc = self.operating_point_idc[self.operating_point];
        let (input, frame_width_bits_minus_1) = frame_width_bits_minus_1(input)?;
        let (input, frame_height_bits_minus_1) = frame_height_bits_minus_1(input)?;
        let (input, max_frame_width_minus_1) =
            max_frame_width_minus_1(input, frame_width_bits_minus_1 + 1)?;
        self.max_frame_width_minus_1 = max_frame_width_minus_1;
        let (mut input, max_frame_height_minus_1) =
            max_frame_height_minus_1(input, frame_height_bits_minus_1 + 1)?;
        self.max_frame_height_minus_1 = max_frame_height_minus_1;
        self.frame_id_numbers_present_flag = if reduced_still_picture_header {
            false
        } else {
            let (rem, result) = frame_id_numbers_present_flag(input)?;
            input = rem;
            result.1
        };
        if self.frame_id_numbers_present_flag {
            let (rem, result) = delta_frame_id_length_minus_2(input)?;
            input = rem;
            self.delta_frame_id_length_minus_2 = result.1;
            let (rem, result) = additional_frame_id_length_minus_1(input)?;
            input = rem;
            self.additional_frame_id_length_minus_1 = result.1;
        }
        let (input, result) = use_128x128_superblock(input)?;
        self.use_128x128_superblock = result;
        let (input, result) = enable_filter_intra(input)?;
        self.enable_filter_intra = result;
        let (mut input, result) = enable_intra_edge_filter(input)?;
        self.enable_intra_edge_filter = result;

        if reduced_still_picture_header {
            self.enable_interintra_compound = false;
            self.enable_masked_compound = false;
            self.enable_warped_motion = false;
            self.enable_dual_filter = false;
            self.enable_order_hint = false;
            self.enable_jnt_comp = false;
            self.enable_ref_frame_mvs = false;
            self.seq_for_screen_content_tools = SelectScreenContentTools;
            self.seq_force_integer_mv = SelectIntegerMv;
            self.order_hint_bits = 0;
        } else {
            let (rem, result) = enable_interintra_compound(input)?;
            input = rem;
            self.enable_interintra_compound = result.1;
            let (rem, result) = enable_masked_compound(input)?;
            input = rem;
            self.enable_masked_compound = result.1;
            let (rem, result) = enable_warped_motion(input)?;
            input = rem;
            self.enable_warped_motion = result.1;
            let (rem, result) = enable_dual_filter(input)?;
            input = rem;
            self.enable_dual_filter = result.1;
            let (rem, result) = enable_order_hint(input)?;
            input = rem;
            self.enable_order_hint = result.1;

            if self.enable_order_hint {
                let (rem, result) = enable_jnt_comp(input)?;
                input = rem;
                self.enable_jnt_comp = result.1;
                let (rem, result) = enable_ref_frame_mvs(input)?;
                input = rem;
                self.enable_ref_frame_mvs = result.1;
            } else {
                self.enable_jnt_comp = false;
                self.enable_ref_frame_mvs = false;
            }
            todo!();
            self.seq_for_screen_content_tools = SelectScreenContentTools;
            self.seq_force_integer_mv = SelectIntegerMv;
            self.order_hint_bits = 0;
        }
        todo!("the stuff after the big if/else")
    }
}
