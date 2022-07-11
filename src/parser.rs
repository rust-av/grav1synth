use anyhow::{anyhow, Result};
use ffmpeg::Rational;
use nom::Finish;

use self::{
    frame::{FrameHeader, RefType, NUM_REF_FRAMES, REFS_PER_FRAME},
    grain::FilmGrainHeader,
    obu::Obu,
    sequence::SequenceHeader,
};
use crate::reader::BitstreamReader;

pub mod frame;
pub mod grain;
pub mod obu;
pub mod sequence;
pub mod tile_group;
pub mod util;

pub struct BitstreamParser<const WRITE: bool> {
    reader: BitstreamReader,
    parsed: bool,
    size: usize,
    seen_frame_header: bool,
    sequence_header: Option<SequenceHeader>,
    previous_frame_header: Option<FrameHeader>,
    ref_frame_idx: [usize; REFS_PER_FRAME],
    ref_order_hint: [u64; NUM_REF_FRAMES],
    big_ref_order_hint: [u64; NUM_REF_FRAMES],
    big_ref_valid: [bool; NUM_REF_FRAMES],
    big_order_hints: [u64; RefType::Last as usize + REFS_PER_FRAME],
    grain_headers: Vec<FilmGrainHeader>,
}

impl<const WRITE: bool> BitstreamParser<WRITE> {
    #[must_use]
    pub fn new(reader: BitstreamReader) -> Self {
        Self {
            reader,
            parsed: Default::default(),
            size: Default::default(),
            seen_frame_header: Default::default(),
            sequence_header: Default::default(),
            previous_frame_header: Default::default(),
            ref_frame_idx: Default::default(),
            ref_order_hint: Default::default(),
            big_ref_order_hint: Default::default(),
            big_ref_valid: Default::default(),
            big_order_hints: Default::default(),
            grain_headers: Default::default(),
        }
    }

    pub fn get_frame_rate(&mut self) -> Result<Rational> {
        Ok(self.reader.get_video_stream()?.avg_frame_rate())
    }

    pub fn get_grain_headers(&mut self) -> Result<&[FilmGrainHeader]> {
        if self.parsed {
            return Ok(&self.grain_headers);
        }

        while let Some(packet) = self.reader.read_packet() {
            if let Some(mut input) = packet.data() {
                loop {
                    let (inner_input, obu) = self
                        .parse_obu(input)
                        .finish()
                        .map_err(|e| anyhow!("{:?}", e))?;
                    input = inner_input;
                    match obu {
                        Some(Obu::SequenceHeader(obu)) => {
                            self.sequence_header = Some(obu);
                        }
                        Some(Obu::FrameHeader(obu)) => {
                            self.grain_headers.push(obu.film_grain_params.clone());
                            self.previous_frame_header = Some(obu);
                        }
                        None => (),
                    };
                    if input.is_empty() {
                        break;
                    }
                }
            } else {
                break;
            }
        }

        Ok(&self.grain_headers)
    }
}
