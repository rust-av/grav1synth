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
use bitvec::{vec::BitVec, BitArr};
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

pub struct ParserContext {
    timing_info: Option<TimingInfo>,
    decoder_model_info: Option<DecoderModelInfo>,
    initial_display_delay_present_flag: bool,
    operating_points_cnt_minus_1: usize,
    operating_point: usize,
    operating_point_idc: Vec<BitArr!(for 12)>,
    seq_level_idx: Vec<u8>,
    seq_tier: Vec<bool>,
    decoder_model_present_for_this_op: BitVec,
    initial_display_delay_present_for_this_op: BitVec,
    initial_display_delay_minus_1: Vec<usize>,
}

impl ParserContext {
    /// We should make a new one of these for each keyframe
    pub fn new() -> Self {
        Self {
            timing_info: None,
            decoder_model_info: None,
            initial_display_delay_present_flag: false,
            operating_points_cnt_minus_1: 0,
            operating_point: 0,
            operating_point_idc: Vec::new(),
            seq_level_idx: Vec::new(),
            seq_tier: Vec::new(),
            decoder_model_present_for_this_op: BitVec::new(),
            initial_display_delay_present_for_this_op: BitVec::new(),
            initial_display_delay_minus_1: Vec::new(),
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
