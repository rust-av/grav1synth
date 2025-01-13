use std::cmp::Ordering;

use anyhow::{anyhow, Result};
use ffmpeg::{codec, encoder, format::context::Output, media, Packet, Rational, Stream};
use log::{debug, warn};
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

const FF_TO_AV1_TS_SHIFT: u64 = 10_000_000 / 1_000;

pub struct BitstreamParser<const WRITE: bool> {
    // Borrow checker REEEE
    reader: Option<BitstreamReader>,
    writer: Option<Output>,
    packet_out: Vec<u8>,
    incoming_grain_header: Option<Vec<GrainTableSegment>>,
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
            reader: Some(reader),
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
            incoming_grain_header: None,
        }
    }

    #[must_use]
    pub fn with_writer(
        reader: BitstreamReader,
        writer: Output,
        incoming_frame_header: Option<Vec<GrainTableSegment>>,
    ) -> Self {
        assert!(
            WRITE,
            "Can only create a BitstreamParser with writer if the WRITE generic is true"
        );

        Self {
            reader: Some(reader),
            writer: Some(writer),
            incoming_grain_header: incoming_frame_header,
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

    pub fn get_grain_headers(&mut self) -> Result<&[FilmGrainHeader]> {
        if self.parsed {
            return Ok(&self.grain_headers);
        }

        let mut reader = self.reader.take().unwrap();
        let stream_idx = reader.get_video_stream()?.index();
        for (stream, packet) in reader.input().packets().filter_map(Result::ok) {
            if let Some(mut input) = packet.data() {
                if stream.index() != stream_idx {
                    continue;
                }

                // ffmpeg gives us the packet in milliseconds.
                // we need it to be in 10,000,000ths of a second.
                let packet_ts = packet.pts().unwrap_or_default() as u64 * FF_TO_AV1_TS_SHIFT;
                loop {
                    let (inner_input, obu) = self
                        .parse_obu(input, packet_ts)
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

    pub fn modify_grain_headers(&mut self) -> Result<()> {
        assert!(
            WRITE,
            "Can only modify headers if the WRITE generic is true"
        );

        if self.parsed {
            warn!("Already called modify_grain_headers--calling it again does nothing");
            return Ok(());
        }

        let mut reader = self.reader.take().unwrap();
        let stream_idx = reader.get_video_stream()?.index();
        let ictx = reader.input();
        let mut stream_mapping = vec![0; ictx.nb_streams() as _];
        let mut ist_time_bases = vec![Rational(0, 1); ictx.nb_streams() as _];
        let mut ost_index = 0;
        for (ist_index, ist) in ictx.streams().enumerate() {
            let ist_medium = ist.parameters().medium();
            if ist_medium != media::Type::Audio
                && ist_medium != media::Type::Video
                && ist_medium != media::Type::Subtitle
            {
                stream_mapping[ist_index] = -1;
                continue;
            }
            stream_mapping[ist_index] = ost_index;
            ist_time_bases[ist_index] = ist.time_base();
            ost_index += 1isize;
            let mut ost = self
                .writer
                .as_mut()
                .unwrap()
                .add_stream(encoder::find(codec::Id::None))
                .unwrap();
            ost.set_parameters(ist.parameters());
            // SAFETY: We need to set codec_tag to 0 lest we run into incompatible codec tag
            // issues when muxing into a different container format. Unfortunately
            // there's no high level API to do this (yet).
            unsafe {
                (*ost.parameters_mut().as_mut_ptr()).codec_tag = 0;
            }
        }

        self.writer
            .as_mut()
            .unwrap()
            .set_metadata(ictx.metadata().to_owned());
        self.writer.as_mut().unwrap().write_header()?;

        for (stream, mut packet) in ictx.packets().filter_map(Result::ok) {
            if let Some(mut input) = packet.data() {
                if stream.index() != stream_idx {
                    self.write_packet(packet, &stream, &stream_mapping, &ist_time_bases)?;
                    continue;
                }

                // ffmpeg gives us the packet in milliseconds.
                // we need it to be in 10,000,000ths of a second.
                let packet_ts = packet.pts().unwrap_or_default() as u64 * FF_TO_AV1_TS_SHIFT;

                loop {
                    let (inner_input, obu) = self
                        .parse_obu(input, packet_ts)
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

                let orig_size = packet.size();
                match self.packet_out.len().cmp(&orig_size) {
                    Ordering::Greater => {
                        debug!(
                            "Growing packet from {} to {}",
                            orig_size,
                            self.packet_out.len()
                        );
                        // `av_grow_packet` takes the number of bytes to grow by.
                        packet.grow(self.packet_out.len() - orig_size);
                    }
                    Ordering::Less => {
                        debug!(
                            "Shrinking packet from {} to {}",
                            orig_size,
                            self.packet_out.len()
                        );
                        // `av_shrink_packet` takes the new size of the packet.
                        // because consistency.
                        packet.shrink(self.packet_out.len());
                    }
                    Ordering::Equal => {
                        debug!("Packet sizes equal at {}", orig_size);
                    }
                }
                packet.data_mut().unwrap().copy_from_slice(&self.packet_out);
                self.write_packet(packet, &stream, &stream_mapping, &ist_time_bases)?;
                self.packet_out.clear();
            } else {
                break;
            }
        }

        self.writer.as_mut().unwrap().write_trailer().unwrap();
        self.parsed = true;

        Ok(())
    }

    fn write_packet(
        &mut self,
        mut packet: Packet,
        stream: &Stream,
        stream_mapping: &[isize],
        ist_time_bases: &[Rational],
    ) -> Result<()> {
        let ist_index = stream.index();
        let ost_index = stream_mapping[ist_index];
        if ost_index < 0 {
            return Ok(());
        }
        let ost = self
            .writer
            .as_mut()
            .unwrap()
            .stream(ost_index as _)
            .unwrap();
        packet.rescale_ts(ist_time_bases[ist_index], ost.time_base());
        packet.set_position(-1);
        packet.set_stream(ost_index as _);
        packet.write_interleaved(self.writer.as_mut().unwrap())?;
        Ok(())
    }
}
