mod obu;
mod spec;
mod util;

use std::path::Path;

use anyhow::{bail, Result};
use av_format::{
    buffer::AccReader,
    demuxer::{Context as DemuxerContext, Event},
};
use av_ivf::demuxer::IvfDemuxer;
use bitvec::vec::BitVec;
use nom::IResult;

pub struct BitstreamParser {
    demuxer: DemuxerContext,
}

impl BitstreamParser {
    pub fn open<P: AsRef<Path>>(input: P) -> Result<Self> {
        let input = std::fs::File::open(input).unwrap();
        let acc = AccReader::new(input);
        let mut demuxer = DemuxerContext::new(Box::new(IvfDemuxer::new()), Box::new(acc));
        demuxer.read_headers()?;

        Ok(Self { demuxer })
    }

    pub fn read_packet(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            match self.demuxer.read_event()? {
                Event::NewPacket(packet) => {
                    return Ok(Some(packet.data));
                }
                Event::Continue | Event::MoreDataNeeded(_) => {
                    continue;
                }
                Event::Eof => {
                    return Ok(None);
                }
                Event::NewStream(_) => {
                    bail!("Only one stream per ivf file is supported");
                }
                _ => {
                    unimplemented!("non-exhaustive enum");
                }
            }
        }
    }
}

#[derive(Default)]
pub struct ParserContext {
    timing_info: Option<TimingInfo>,
    decoder_model_info: Option<DecoderModelInfo>,
    initial_display_delay_present_flag: bool,
    operating_points_cnt_minus_1: u32,
    operating_point: u32,
    operating_point_idc: Vec<BitVec>,
    seq_level_idx: Vec<u8>,
    seq_tier: Vec<bool>,
    decoder_model_present_for_this_op: BitVec,
    initial_display_delay_present_for_this_op: BitVec,
    initial_display_delay_minus_1: Vec<u32>,
    seq_profile: u8,
    still_picture: bool,
    max_frame_width_minus_1: u32,
    max_frame_height_minus_1: u32,
    frame_id_numbers_present_flag: bool,
    delta_frame_id_length_minus_2: u32,
    additional_frame_id_length_minus_1: u32,
    use_128x128_superblock: bool,
    enable_filter_intra: bool,
    enable_intra_edge_filter: bool,
    enable_interintra_compound: bool,
    enable_masked_compound: bool,
    enable_warped_motion: bool,
    enable_dual_filter: bool,
    enable_order_hint: bool,
    enable_jnt_comp: bool,
    enable_ref_frame_mvs: bool,
    seq_force_screen_content_tools: bool,
    seq_force_integer_mv: bool,
    order_hint_bits: u32,
    enable_superres: bool,
    enable_cdef: bool,
    enable_restoration: bool,
    color_config: ColorConfig,
    film_grain_params_present: bool,
}

impl ParserContext {
    /// We should make a new one of these for each keyframe
    pub fn new() -> Self {
        Self {
            operating_point_idc: vec![BitVec::repeat(false, 12)],
            ..Default::default()
        }
    }

    /// Parses an OBU packet (usually one frame + headers).
    pub fn full_parse<'a>(
        &'a mut self,
        mut input: (&'a [u8], usize),
        size: usize,
    ) -> IResult<(&[u8], usize), ()> {
        while !input.0.is_empty() {
            input = self.parse_obu(input, size)?.0;
        }
        Ok((input, ()))
    }
}
