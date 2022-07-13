use anyhow::{anyhow, Result};
use ffmpeg::{
    format::context::Output,
    packet::Ref,
    sys::av_interleaved_write_frame,
    Packet,
    Rational,
};
use log::warn;
use nom::Finish;

use self::{
    frame::{FrameHeader, RefType, NUM_REF_FRAMES, REFS_PER_FRAME},
    grain::FilmGrainHeader,
    obu::Obu,
    sequence::SequenceHeader,
};
use crate::{reader::BitstreamReader, GrainTableSegment};

pub mod frame;
pub mod grain;
pub mod obu;
pub mod sequence;
pub mod tile_group;
pub mod util;

pub struct BitstreamParser<const WRITE: bool> {
    reader: BitstreamReader,
    writer: Option<Output>,
    packet_out: Vec<u8>,
    incoming_frame_header: Option<Vec<GrainTableSegment>>,
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
        assert!(
            !WRITE,
            "Attempted to create a BitstreamReader with WRITE set to true, but without a writer. \
             Probably not what you want."
        );

        Self {
            reader,
            writer: None,
            packet_out: Vec::new(),
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
            incoming_frame_header: None,
        }
    }

    #[must_use]
    pub fn with_writer(
        mut reader: BitstreamReader,
        mut writer: Output,
        incoming_frame_header: Option<Vec<GrainTableSegment>>,
    ) -> Self {
        assert!(
            WRITE,
            "Can only create a BitstreamParser with writer if the WRITE generic is true"
        );

        writer.set_metadata(reader.input().metadata().to_owned());
        writer.write_header().unwrap();

        Self {
            reader,
            writer: Some(writer),
            incoming_frame_header,
            packet_out: Vec::new(),
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

        self.parsed = true;

        Ok(&self.grain_headers)
    }

    pub fn remove_grain_headers(&mut self) -> Result<()> {
        assert!(
            WRITE,
            "Can only remove headers if the WRITE generic is true"
        );

        if self.parsed {
            warn!("Already called remove_grain_headers--calling it again does nothing");
            return Ok(());
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
                            self.previous_frame_header = Some(obu);
                        }
                        None => (),
                    };
                    if input.is_empty() {
                        break;
                    }
                }

                {
                    let packet = Packet::borrow(&self.packet_out);
                    // SAFETY: Performs FFI. This is the same code used by
                    // `Packet::write_interleaved`, but we can't use that directly
                    // without using `Packet::copy` (which, of course, involves copying data)
                    // because the `Borrow` interface only lets us access a raw `AVPacket` and not
                    // the Rustified `Packet` struct.
                    unsafe {
                        match av_interleaved_write_frame(
                            self.writer.as_mut().unwrap().as_mut_ptr(),
                            packet.as_ptr() as *mut _,
                        ) {
                            0i32 => Ok(()),
                            e => Err(ffmpeg::Error::from(e)),
                        }?;
                    }
                }
                self.packet_out.clear();
            } else {
                break;
            }
        }

        self.parsed = true;

        todo!("Remux the video into the container");

        Ok(())
    }
}
