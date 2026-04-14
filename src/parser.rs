use std::cmp::Ordering;

use anyhow::{Result, anyhow};
use ffmpeg::{
    Dictionary, Packet, Rational, Stream, codec, encoder, format::context::Output, media,
};
use log::{debug, log_enabled, warn};
use nom::Finish;

use self::{
    frame::{FrameHeader, NUM_REF_FRAMES, REFS_PER_FRAME, RefType},
    grain::FilmGrainHeader,
    obu::Obu,
    sequence::SequenceHeader,
};
use crate::{GrainTableSegment, reader::BitstreamReader};

pub mod frame;
pub mod grain;
pub mod obu;
pub mod sequence;
pub mod tile_group;
pub mod trace;
pub mod util;

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

    fn ffmpeg_pts_to_av1_ts(pts: i64, time_base: Rational) -> u64 {
        if pts < 0 {
            return 0;
        }

        let pts = pts as u64;
        let num = time_base.0 as u64;
        let den = time_base.1 as u64;
        if den == 0 {
            return 0;
        }

        // Use ceiling so timestamps line up with the 100ns packet boundaries that
        // are emitted by `aggregate_grain_headers` when generating the grain table.
        (pts * num * 10_000_000u64).div_ceil(den)
    }

    /// Returns `true` if the `film_grain_params_present` flag is set in the stream's Sequence
    /// Header, and `false` otherwise.
    ///
    /// This reads only as far as the first Sequence Header OBU — typically the very start of
    /// the first video packet — so it is far faster than [`Self::get_grain_headers`] for a
    /// simple presence check.
    pub fn film_grain_params_present(&mut self) -> Result<bool> {
        // Reuse an already-parsed sequence header if available.
        if let Some(ref sh) = self.sequence_header {
            return Ok(sh.film_grain_params_present);
        }

        let mut reader = self.reader.take().unwrap();
        let stream_idx = reader.get_video_stream()?.index();

        'packets: for (stream, packet) in reader.input().packets().filter_map(Result::ok) {
            let Some(mut input) = packet.data() else {
                break;
            };
            if stream.index() != stream_idx {
                continue;
            }
            loop {
                let (remaining, obu) = self
                    .parse_obu(input, 0)
                    .finish()
                    .map_err(|e| anyhow!("{e:?}"))?;
                input = remaining;
                if let Some(Obu::SequenceHeader(sh)) = obu {
                    self.sequence_header = Some(sh);
                    break 'packets;
                }
                if input.is_empty() {
                    break;
                }
            }
        }

        Ok(self
            .sequence_header
            .as_ref()
            .map_or(false, |sh| sh.film_grain_params_present))
    }

    pub fn get_grain_headers(&mut self) -> Result<&[FilmGrainHeader]> {
        if self.parsed {
            return Ok(&self.grain_headers);
        }

        let mut reader = self.reader.take().unwrap();
        let stream = reader.get_video_stream()?;
        let stream_idx = reader.get_video_stream()?.index();
        let stream_time_base = stream.time_base();
        for (stream, packet) in reader.input().packets().filter_map(Result::ok) {
            if let Some(mut input) = packet.data() {
                if stream.index() != stream_idx {
                    continue;
                }

                debug!(
                    target: "trace_headers",
                    "Packet: {} bytes, pts {}, dts {}.",
                    input.len(),
                    packet.pts().unwrap_or_default(),
                    packet.dts().unwrap_or_default(),
                );

                let packet_ts =
                    Self::ffmpeg_pts_to_av1_ts(packet.pts().unwrap_or_default(), stream_time_base);
                loop {
                    let (inner_input, obu) = self
                        .parse_obu(input, packet_ts)
                        .finish()
                        .map_err(|e| anyhow!("{e:?}"))?;
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
                    }
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

        let input_chapters: Vec<(i64, Rational, i64, i64, Dictionary)> = ictx
            .chapters()
            .map(|ch| {
                (
                    ch.id(),
                    ch.time_base(),
                    ch.start(),
                    ch.end(),
                    ch.metadata().to_owned(),
                )
            })
            .collect();

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

            let ist_metadata = ist.metadata().to_owned();
            let ist_sar = ist.sample_aspect_ratio();

            let mut ost = self
                .writer
                .as_mut()
                .unwrap()
                .add_stream(encoder::find(codec::Id::None))
                .unwrap();
            ost.set_parameters(ist.parameters());
            ost.metadata_mut().replace_with(ist_metadata);
            ost.set_sample_aspect_ratio(ist_sar);
            // SAFETY: We need to set codec_tag to 0 and copy disposition flags.
            // There's no high level API for either (yet).
            unsafe {
                (*ost.parameters_mut().as_mut_ptr()).codec_tag = 0;
                (*ost.as_mut_ptr()).disposition = (*ist.as_ptr()).disposition;
            }
        }

        self.writer
            .as_mut()
            .unwrap()
            .metadata_mut()
            .replace_with(ictx.metadata().to_owned());

        for (id, time_base, start, end, metadata) in input_chapters {
            let title = metadata.as_ref().get("title").unwrap_or("").to_owned();
            let mut out_chapter = self
                .writer
                .as_mut()
                .unwrap()
                .add_chapter(id, time_base, start, end, &title)?;
            out_chapter.metadata_mut().replace_with(metadata);
        }

        let video_stream_time_base = ictx.stream(stream_idx as _).unwrap().time_base();

        self.writer.as_mut().unwrap().write_header()?;

        for (stream, mut packet) in ictx.packets().filter_map(Result::ok) {
            if let Some(mut input) = packet.data() {
                if stream.index() != stream_idx {
                    self.write_packet(
                        packet,
                        &stream,
                        &stream_mapping,
                        &ist_time_bases,
                        stream_idx,
                    )?;
                    continue;
                }

                debug!(
                    target: "trace_headers",
                    "Packet: {} bytes, pts {}, dts {}.",
                    input.len(),
                    packet.pts().unwrap_or_default(),
                    packet.dts().unwrap_or_default(),
                );

                let packet_ts = Self::ffmpeg_pts_to_av1_ts(
                    packet.pts().unwrap_or_default(),
                    video_stream_time_base,
                );

                loop {
                    let (inner_input, obu) = self
                        .parse_obu(input, packet_ts)
                        .finish()
                        .map_err(|e| anyhow!("{e:?}"))?;
                    input = inner_input;
                    match obu {
                        Some(Obu::SequenceHeader(obu)) => {
                            self.sequence_header = Some(obu);
                        }
                        Some(Obu::FrameHeader(obu)) => {
                            self.previous_frame_header = Some(obu);
                        }
                        None => (),
                    }
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
                        debug!("Packet sizes equal at {orig_size}");
                    }
                }
                packet.data_mut().unwrap().copy_from_slice(&self.packet_out);
                self.write_packet(
                    packet,
                    &stream,
                    &stream_mapping,
                    &ist_time_bases,
                    stream_idx,
                )?;
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
        video_stream_idx: usize,
    ) -> Result<()> {
        let ist_index = stream.index();
        let ost_index = stream_mapping[ist_index];
        if ost_index < 0 {
            return Ok(());
        }

        if log_enabled!(target: "trace_headers", log::Level::Debug)
            && ist_index == video_stream_idx
            && let Some(data) = packet.data()
        {
            debug!(
                target: "trace_headers",
                "=== Re-parsing modified packet: {} bytes, pts {}, dts {} ===",
                data.len(),
                packet.pts().unwrap_or_default(),
                packet.dts().unwrap_or_default(),
            );
            let packet_ts =
                Self::ffmpeg_pts_to_av1_ts(packet.pts().unwrap_or_default(), stream.time_base());
            let mut read_parser = BitstreamParser::<false> {
                reader: None,
                writer: None,
                packet_out: Vec::new(),
                incoming_grain_header: None,
                parsed: false,
                size: self.size,
                seen_frame_header: self.seen_frame_header,
                sequence_header: self.sequence_header.clone(),
                previous_frame_header: self.previous_frame_header.clone(),
                ref_frame_idx: self.ref_frame_idx,
                ref_order_hint: self.ref_order_hint,
                big_ref_order_hint: self.big_ref_order_hint,
                big_ref_valid: self.big_ref_valid,
                big_order_hints: self.big_order_hints,
                grain_headers: Vec::new(),
            };
            let mut input = data;
            loop {
                match read_parser.parse_obu(input, packet_ts).finish() {
                    Ok((remaining, _)) => {
                        input = remaining;
                        if input.is_empty() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Debug re-parse of modified packet failed: {e:?}");
                        break;
                    }
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    // ===== Helpers =====

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

    fn make_parsed_parser<const WRITE: bool>(
        headers: Vec<FilmGrainHeader>,
    ) -> BitstreamParser<WRITE> {
        BitstreamParser {
            reader: None,
            writer: None,
            packet_out: Vec::new(),
            incoming_grain_header: None,
            parsed: true,
            size: 0,
            seen_frame_header: false,
            sequence_header: None,
            previous_frame_header: None,
            ref_frame_idx: Default::default(),
            ref_order_hint: Default::default(),
            big_ref_order_hint: Default::default(),
            big_ref_valid: Default::default(),
            big_order_hints: Default::default(),
            grain_headers: headers,
        }
    }

    fn sample_grain_params() -> grain::FilmGrainParams {
        grain::FilmGrainParams {
            grain_seed: 42,
            scaling_points_y: Default::default(),
            scaling_points_cb: Default::default(),
            scaling_points_cr: Default::default(),
            scaling_shift: 8,
            ar_coeff_lag: 0,
            ar_coeffs_y: Default::default(),
            ar_coeffs_cb: Default::default(),
            ar_coeffs_cr: Default::default(),
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

    // ===== Part 1: Pure Unit Tests =====

    #[test]
    fn get_grain_headers_returns_cached_when_already_parsed() {
        let headers = vec![
            FilmGrainHeader::Disable,
            FilmGrainHeader::UpdateGrain(sample_grain_params()),
        ];
        let mut parser = make_parsed_parser::<false>(headers);

        let result = parser
            .get_grain_headers()
            .expect("should return cached headers");

        assert_eq!(result.len(), 2);
        assert_eq!(result[0], FilmGrainHeader::Disable);
    }

    #[test]
    fn get_grain_headers_returns_empty_when_parsed_with_no_grain() {
        let mut parser = make_parsed_parser::<false>(Vec::new());

        let result = parser.get_grain_headers().expect("should return empty");

        assert!(result.is_empty());
    }

    #[test]
    fn get_grain_headers_preserves_all_grain_variants() {
        let headers = vec![
            FilmGrainHeader::Disable,
            FilmGrainHeader::CopyRefFrame,
            FilmGrainHeader::UpdateGrain(sample_grain_params()),
        ];
        let mut parser = make_parsed_parser::<false>(headers);

        let result = parser
            .get_grain_headers()
            .expect("should preserve variants");

        assert_eq!(result.len(), 3);
        assert_eq!(result[0], FilmGrainHeader::Disable);
        assert_eq!(result[1], FilmGrainHeader::CopyRefFrame);
        assert!(matches!(result[2], FilmGrainHeader::UpdateGrain(_)));
    }

    #[test]
    fn get_grain_headers_second_call_returns_same_result() {
        let headers = vec![FilmGrainHeader::CopyRefFrame, FilmGrainHeader::Disable];
        let mut parser = make_parsed_parser::<false>(headers);

        let first = parser.get_grain_headers().expect("first call").to_vec();
        let second = parser.get_grain_headers().expect("second call").to_vec();

        assert_eq!(first, second);
    }

    #[test]
    #[should_panic]
    fn get_grain_headers_panics_when_reader_is_none() {
        let mut parser = make_parser::<false>();
        // parsed=false, reader=None → .take().unwrap() panics
        let _ = parser.get_grain_headers();
    }

    #[test]
    #[should_panic(expected = "Can only modify headers")]
    fn modify_grain_headers_panics_when_write_is_false() {
        let mut parser = make_parser::<false>();
        let _ = parser.modify_grain_headers();
    }

    #[test]
    fn modify_grain_headers_returns_ok_when_already_parsed() {
        let mut parser = make_parsed_parser::<true>(Vec::new());

        let result = parser.modify_grain_headers();

        assert!(result.is_ok());
    }

    #[test]
    #[should_panic]
    fn modify_grain_headers_panics_when_reader_is_none() {
        let mut parser = make_parser::<true>();
        // WRITE=true, parsed=false, reader=None → .take().unwrap() panics
        let _ = parser.modify_grain_headers();
    }

    // ===== Part 2: I/O Tests =====

    #[cfg(feature = "dav1d_tests")]
    mod io {
        use super::*;
        use crate::reader::BitstreamReader;
        use std::path::PathBuf;

        fn test_data_path(relative: &str) -> PathBuf {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("dav1d-test-data")
                .join(relative)
        }

        #[test]
        fn new_creates_read_only_parser() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let parser = BitstreamParser::<false>::new(reader);
            assert!(!parser.parsed);
        }

        #[test]
        #[should_panic(expected = "WRITE set to true")]
        fn new_panics_when_write_is_true() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let _ = BitstreamParser::<true>::new(reader);
        }

        #[test]
        fn with_writer_creates_write_parser() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let output = tempfile::Builder::new().suffix(".ivf").tempfile().unwrap();
            let writer = ffmpeg::format::output(output.path()).expect("output should open");

            let parser = BitstreamParser::<true>::with_writer(reader, writer, None);

            assert!(!parser.parsed);
            assert!(parser.writer.is_some());
            assert!(parser.incoming_grain_header.is_none());
        }

        #[test]
        fn with_writer_stores_incoming_grain_header() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let output = tempfile::Builder::new().suffix(".ivf").tempfile().unwrap();
            let writer = ffmpeg::format::output(output.path()).expect("output should open");
            let segments = vec![];

            let parser = BitstreamParser::<true>::with_writer(reader, writer, Some(segments));

            assert!(parser.incoming_grain_header.is_some());
        }

        #[test]
        #[should_panic(expected = "WRITE generic is true")]
        fn with_writer_panics_when_write_is_false() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let output = tempfile::Builder::new().suffix(".ivf").tempfile().unwrap();
            let writer = ffmpeg::format::output(output.path()).expect("output should open");
            let _ = BitstreamParser::<false>::with_writer(reader, writer, None);
        }

        #[test]
        fn get_grain_headers_parses_valid_file() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let mut parser = BitstreamParser::<false>::new(reader);

            let result = parser.get_grain_headers();

            assert!(result.is_ok());
            assert!(!result.unwrap().is_empty());
        }

        #[test]
        fn get_grain_headers_sets_parsed_flag() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let mut parser = BitstreamParser::<false>::new(reader);
            assert!(!parser.parsed);

            let _ = parser.get_grain_headers().expect("should parse");

            assert!(parser.parsed);
        }

        #[test]
        fn get_grain_headers_consumes_reader_then_caches() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let mut parser = BitstreamParser::<false>::new(reader);

            let first = parser.get_grain_headers().expect("first call").to_vec();
            assert!(parser.reader.is_none(), "reader should be consumed");

            let second = parser
                .get_grain_headers()
                .expect("second call (cached)")
                .to_vec();
            assert_eq!(first, second);
        }

        #[test]
        fn get_grain_headers_stores_sequence_header() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let mut parser = BitstreamParser::<false>::new(reader);

            let _ = parser.get_grain_headers().expect("should parse");

            assert!(parser.sequence_header.is_some());
        }

        #[test]
        fn get_grain_headers_stores_previous_frame_header() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let mut parser = BitstreamParser::<false>::new(reader);

            let _ = parser.get_grain_headers().expect("should parse");

            assert!(parser.previous_frame_header.is_some());
        }

        #[test]
        fn modify_grain_headers_processes_file() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let output = tempfile::Builder::new().suffix(".ivf").tempfile().unwrap();
            let writer = ffmpeg::format::output(output.path()).expect("output should open");
            let mut parser = BitstreamParser::<true>::with_writer(reader, writer, None);

            let result = parser.modify_grain_headers();

            assert!(result.is_ok());
            let metadata = std::fs::metadata(output.path()).expect("output file should exist");
            assert!(metadata.len() > 0, "output file should be non-empty");
        }

        #[test]
        fn modify_grain_headers_sets_parsed_flag() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let output = tempfile::Builder::new().suffix(".ivf").tempfile().unwrap();
            let writer = ffmpeg::format::output(output.path()).expect("output should open");
            let mut parser = BitstreamParser::<true>::with_writer(reader, writer, None);
            assert!(!parser.parsed);

            let _ = parser.modify_grain_headers().expect("should process");

            assert!(parser.parsed);
        }

        #[test]
        fn modify_grain_headers_does_not_populate_grain_headers() {
            let reader = BitstreamReader::open(test_data_path("8-bit/data/00000000.ivf"))
                .expect("test file should open");
            let output = tempfile::Builder::new().suffix(".ivf").tempfile().unwrap();
            let writer = ffmpeg::format::output(output.path()).expect("output should open");
            let mut parser = BitstreamParser::<true>::with_writer(reader, writer, None);

            let _ = parser.modify_grain_headers().expect("should process");

            assert!(
                parser.grain_headers.is_empty(),
                "write path should not collect grain headers"
            );
        }
    }
}
