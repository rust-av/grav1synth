use log::{debug, trace};
use nom::{
    IResult, Parser,
    bits::{bits, complete as bit_parsers},
    error::{Error, context},
};
use num_enum::TryFromPrimitive;

use crate::misc::to_binary_string;

use super::{
    BitstreamParser,
    frame::FrameHeader,
    sequence::SequenceHeader,
    trace::{
        TraceCtx, trace_bool, trace_field, trace_leb128, trace_section, trace_take_u8,
        trace_zero_bit,
    },
    util::{BitInput, leb128_write},
};

impl<const WRITE: bool> BitstreamParser<WRITE> {
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::cognitive_complexity)]
    /// Parse a single AV1 OBU from `input` and optionally emit a high-level parsed payload.
    ///
    /// This is the packet-level dispatcher for OBU parsing. It reads the OBU header and size,
    /// enforces operating-point layer filtering, then delegates payload parsing to the
    /// OBU-specific handlers. In write mode (`WRITE = true`), it mirrors bytes into
    /// `self.packet_out` and rewrites size fields when transformed payload lengths differ from
    /// the original bitstream.
    ///
    /// `packet_ts` uses FFmpeg's 10,000,000 tick-per-second time base and is forwarded to
    /// frame-level parsing.
    ///
    /// # Returns
    /// - Remaining unconsumed input.
    /// - `Some(Obu)` for parsed sequence/frame header payloads that are surfaced to callers.
    /// - `None` for OBUs that are intentionally skipped or passed through.
    ///
    /// # Errors
    /// Returns a `nom` parser error when any required OBU header, size, or payload parse fails.
    pub fn parse_obu<'a>(
        &mut self,
        input: &'a [u8],
        // Once again, this is in 10,000,000ths of a second
        packet_ts: u64,
    ) -> IResult<&'a [u8], Option<Obu>, Error<&'a [u8]>> {
        debug!("Parsing OBU from remaining data of {} bytes", input.len());
        trace_section("OBU header");
        let pre_input = input;
        let packet_start_len = self.packet_out.len();
        let (input, (obu_header, header_bits)) =
            context("Failed parsing obu header", parse_obu_header).parse(input)?;
        let obu_header_size = if obu_header.extension.is_some() { 2 } else { 1 };
        let obu_size_pos = packet_start_len + obu_header_size;
        let mut leb_size = 0;
        let (input, obu_size) = if obu_header.has_size_field {
            let (input, result) = context("Failed parsing obu size", |input| {
                trace_leb128(input, header_bits, "obu_size")
            })
            .parse(input)?;
            leb_size = result.bytes_read;
            debug!("Parsed OBU size of {}", result.value);
            (input, result.value as usize)
        } else {
            debug_assert!(self.size > 0);
            (
                input,
                self.size - 1 - usize::from(obu_header.extension.is_some()),
            )
        };
        debug!("Parsing contents of OBU of size {obu_size}");
        self.size = obu_size;
        if WRITE {
            let total_header_size = pre_input.len() - input.len();
            self.packet_out
                .extend_from_slice(&pre_input[..total_header_size]);
            debug!(
                "Writing header of size {} to packet_out, total packet size at {}",
                total_header_size,
                self.packet_out.len()
            );
            trace!(
                "Packet contents: {}",
                to_binary_string(&pre_input[..total_header_size])
            );
        }

        if obu_header.obu_type != ObuType::SequenceHeader
            && obu_header.obu_type != ObuType::TemporalDelimiter
            && let Some(ref obu_ext) = obu_header.extension
            && let Some(ref sequence_header) = self.sequence_header
        {
            let op_pt_idc = sequence_header.cur_operating_point_idc;
            if op_pt_idc != 0 {
                let in_temporal_layer = (op_pt_idc >> obu_ext.temporal_id) & 1 > 0;
                let in_spatial_layer = (op_pt_idc >> (obu_ext.spatial_id + 8)) & 1 > 0;
                if !in_temporal_layer || !in_spatial_layer {
                    if WRITE {
                        self.packet_out.extend_from_slice(&input[..obu_size]);
                        debug!(
                            "Writing skipped OBU of size {} to packet_out, total packet \
                                     size at {}",
                            obu_size,
                            self.packet_out.len()
                        );
                    }
                    debug!("Skipping OBU parsing because not in temporal or spatial layer");
                    return Ok((&input[obu_size..], None));
                }
            }
        }

        let obu_bit_offset = header_bits + leb_size * 8;
        match obu_header.obu_type {
            ObuType::SequenceHeader => {
                trace_section("Sequence Header");
                debug!("Parsing sequence header");
                let pre_len = input.len();
                let (mut input, header) = context("Failed parsing sequence header", |input| {
                    // Writing handled within this function
                    self.parse_sequence_header(input, obu_bit_offset)
                })
                .parse(input)?;
                debug!(
                    "Consumed {} bytes of data for sequence header",
                    pre_len - input.len()
                );
                if obu_header.has_size_field {
                    if WRITE {
                        let bytes_written = self.packet_out.len() - packet_start_len;
                        let bytes_taken = pre_input.len() - input.len();
                        let obu_size_change = bytes_written as isize - bytes_taken as isize;
                        if obu_size_change != 0 {
                            self.adjust_obu_size(
                                obu_size_pos,
                                leb_size,
                                (obu_size as isize + obu_size_change) as usize,
                            );
                        }
                    }
                    let adjustment = obu_size - (pre_len - input.len());
                    input = &input[adjustment..];
                }

                Ok((input, Some(Obu::SequenceHeader(header))))
            }
            ObuType::Frame => {
                trace_section("Frame");
                debug!("Parsing frame");
                let pre_len = input.len();
                let (mut input, header) = context("Failed parsing frame obu", |input| {
                    // Writing handled within this function
                    self.parse_frame_obu(input, obu_header, packet_ts, obu_bit_offset)
                })
                .parse(input)?;
                debug!("Consumed {} bytes of data for frame", pre_len - input.len());
                if obu_header.has_size_field {
                    if WRITE {
                        let bytes_written = self.packet_out.len() - packet_start_len;
                        let bytes_taken = pre_input.len() - input.len();
                        let obu_size_change = bytes_written as isize - bytes_taken as isize;
                        if obu_size_change != 0 {
                            self.adjust_obu_size(
                                obu_size_pos,
                                leb_size,
                                (obu_size as isize + obu_size_change) as usize,
                            );
                        }
                    }
                    let adjustment = obu_size - (pre_len - input.len());
                    input = &input[adjustment..];
                }

                Ok((input, header.map(Obu::FrameHeader)))
            }
            ObuType::FrameHeader => {
                trace_section("Frame Header");
                debug!("Parsing frame header");
                let pre_len = input.len();
                let (mut input, header) = context("Failed parsing frame header", |input| {
                    // Writing handled within this function.
                    // RATIONALE: Standalone FrameHeader OBUs use trailing_bits()
                    // (starting with a 1 bit) instead of byte_alignment() (all zeros),
                    // so we skip alignment verification here.
                    self.parse_frame_header(input, obu_header, packet_ts, obu_bit_offset, false)
                })
                .parse(input)?;
                debug!(
                    "Consumed {} bytes of data for frame header",
                    pre_len - input.len()
                );
                if obu_header.has_size_field {
                    if WRITE {
                        let bytes_written = self.packet_out.len() - packet_start_len;
                        let bytes_taken = pre_input.len() - input.len();
                        let obu_size_change = bytes_written as isize - bytes_taken as isize;
                        if obu_size_change != 0 {
                            self.adjust_obu_size(
                                obu_size_pos,
                                leb_size,
                                (obu_size as isize + obu_size_change) as usize,
                            );
                        }
                    }
                    let adjustment = obu_size - (pre_len - input.len());
                    if WRITE {
                        self.packet_out.extend(input.iter().take(adjustment));
                        debug!("Writing adjustment of size {}", adjustment);
                    }
                    input = &input[adjustment..];
                }

                Ok((input, header.map(Obu::FrameHeader)))
            }
            ObuType::TileGroup => {
                // I'm adding an assert here explicitly because I'm not sure if the spec
                // actually requires this. I think it does. But it's 681 pages.
                unreachable!("This should only be called from within a frame OBU.");
            }
            ObuType::TemporalDelimiter => {
                trace_section("Temporal Delimiter");
                debug!("Skipping temporal delimiter");
                self.seen_frame_header = false;
                if WRITE {
                    self.packet_out.extend_from_slice(&input[..obu_size]);
                    debug!(
                        "Writing temporal delimiter of size {} to packet_out, total packet size \
                         at {}",
                        obu_size,
                        self.packet_out.len()
                    );
                }
                Ok((&input[obu_size..], None))
            }
            _ => {
                debug!("Skipping unused OBU type");
                if WRITE {
                    self.packet_out.extend_from_slice(&input[..obu_size]);
                    debug!(
                        "Writing unused OBU of size {} to packet_out, total packet size at {}",
                        obu_size,
                        self.packet_out.len()
                    );
                }
                Ok((&input[obu_size..], None))
            }
        }
    }

    /// Rewrite an OBU size field in `self.packet_out` after write-path payload mutation.
    ///
    /// `pos` is the byte offset of the original LEB128-encoded size, `leb_size` is the number of
    /// bytes currently occupied by that encoding, and `new_obu_size` is the replacement payload
    /// size. The buffer is rebuilt so size fields can grow or shrink without fragile in-place
    /// shifting logic.
    fn adjust_obu_size(&mut self, pos: usize, leb_size: usize, new_obu_size: usize) {
        let encoded_size = leb128_write(new_obu_size as u32);
        trace!("Encoded leb128 packet: {}", to_binary_string(&encoded_size));
        // Add a little padding just in case the leb grew
        let mut new_obu = Vec::with_capacity(self.packet_out.len() + 8);
        new_obu.extend_from_slice(&self.packet_out[..pos]);
        new_obu.extend_from_slice(&encoded_size);
        new_obu.extend_from_slice(&self.packet_out[(pos + leb_size)..]);
        self.packet_out = new_obu;
        debug!("Adjusted packet size to {}", self.packet_out.len());
    }
}

#[derive(Debug, Clone)]
pub enum Obu {
    SequenceHeader(SequenceHeader),
    FrameHeader(FrameHeader),
}

#[derive(Debug, Clone, Copy)]
pub struct ObuHeader {
    pub obu_type: ObuType,
    pub has_size_field: bool,
    pub extension: Option<ObuExtension>,
}

#[derive(Debug, Clone, Copy)]
pub struct ObuExtension {
    pub temporal_id: u8,
    pub spatial_id: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum ObuType {
    Reserved0 = 0,
    SequenceHeader = 1,
    TemporalDelimiter = 2,
    FrameHeader = 3,
    TileGroup = 4,
    Metadata = 5,
    Frame = 6,
    RedundantFrameHeader = 7,
    TileList = 8,
    Reserved9 = 9,
    Reserved10 = 10,
    Reserved11 = 11,
    Reserved12 = 12,
    Reserved13 = 13,
    Reserved14 = 14,
    Padding = 15,
}

/// Parse the fixed AV1 OBU header from byte-aligned input.
///
/// The parser validates required zero bits (`forbidden_bit`, `reserved_1bit`) and conditionally
/// parses the extension byte when `extension_flag` is set.
///
/// Returns the parsed header and the number of header bits consumed (8 without
/// extension, 16 with extension) so callers can compute downstream bit offsets.
fn parse_obu_header(input: &[u8]) -> IResult<&[u8], (ObuHeader, usize), Error<&[u8]>> {
    let (input, result) = bits(|input| {
        let ctx = TraceCtx::new(input, 0);
        let (input, ()) = trace_zero_bit(input, ctx, "obu_forbidden_bit")?;
        let pos = ctx.pos(input);
        let (input, obu_type) = context("Failed parsing obu_type", obu_type).parse(input)?;
        trace_field(pos, "obu_type", 4, u64::from(obu_type as u8));
        let (input, extension_flag) = trace_bool(input, ctx, "obu_extension_flag")?;
        let (input, has_size_field) = trace_bool(input, ctx, "obu_has_size_field")?;
        let (input, ()) = trace_zero_bit(input, ctx, "obu_reserved_1bit")?;

        let (input, extension) = if extension_flag {
            let (input, extension) = obu_extension(input, ctx)?;
            (input, Some(extension))
        } else {
            (input, None)
        };

        let header_bits = if extension.is_some() { 16 } else { 8 };
        Ok((
            input,
            (
                ObuHeader {
                    obu_type,
                    has_size_field,
                    extension,
                },
                header_bits,
            ),
        ))
    })(input)?;

    Ok((input, result))
}

/// Parse the 8-bit OBU extension payload.
///
/// The extension carries `temporal_id` (3 bits) and `spatial_id` (2 bits). The trailing
/// 3 reserved bits are consumed and intentionally discarded.
fn obu_extension<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
) -> IResult<BitInput<'a>, ObuExtension, Error<BitInput<'a>>> {
    let (input, temporal_id) = trace_take_u8(input, ctx, 3, "temporal_id")?;
    let (input, spatial_id) = trace_take_u8(input, ctx, 2, "spatial_id")?;
    let (input, _reserved) = trace_take_u8(input, ctx, 3, "extension_header_reserved_3bits")?;
    Ok((
        input,
        ObuExtension {
            temporal_id,
            spatial_id,
        },
    ))
}

/// Parse a 4-bit OBU type discriminant and convert it into [`ObuType`].
fn obu_type(input: BitInput) -> IResult<BitInput, ObuType, Error<BitInput>> {
    bit_parsers::take(4usize)
        .map_res(|output: u8| ObuType::try_from(output))
        .parse(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_obu_header_byte(
        forbidden_bit: u8,
        obu_type: ObuType,
        extension_flag: bool,
        has_size_field: bool,
        reserved_bit: u8,
    ) -> u8 {
        (forbidden_bit << 7)
            | ((obu_type as u8) << 3)
            | (u8::from(extension_flag) << 2)
            | (u8::from(has_size_field) << 1)
            | reserved_bit
    }

    fn make_obu_extension_byte(temporal_id: u8, spatial_id: u8, reserved: u8) -> u8 {
        (temporal_id << 5) | (spatial_id << 3) | reserved
    }

    #[test]
    fn parse_obu_header_without_extension_parses_expected_fields() {
        let header = make_obu_header_byte(0, ObuType::Frame, false, true, 0);
        let input = [header, 0xAA];

        let (remaining, (parsed, header_bits)) =
            parse_obu_header(&input).expect("header should parse");

        assert_eq!(parsed.obu_type, ObuType::Frame);
        assert!(parsed.has_size_field);
        assert!(parsed.extension.is_none());
        assert_eq!(header_bits, 8);
        assert_eq!(remaining, &input[1..]);
    }

    #[test]
    fn parse_obu_header_with_extension_parses_extension_payload() {
        let header = make_obu_header_byte(0, ObuType::SequenceHeader, true, false, 0);
        let extension = make_obu_extension_byte(5, 2, 0b111);
        let input = [header, extension, 0xAA];

        let (remaining, (parsed, header_bits)) =
            parse_obu_header(&input).expect("header should parse");

        assert_eq!(parsed.obu_type, ObuType::SequenceHeader);
        assert!(!parsed.has_size_field);
        let extension = parsed.extension.expect("extension should be present");
        assert_eq!(extension.temporal_id, 5);
        assert_eq!(extension.spatial_id, 2);
        assert_eq!(header_bits, 16);
        assert_eq!(remaining, &input[2..]);
    }

    #[test]
    fn parse_obu_header_rejects_non_zero_forbidden_bit() {
        let header = make_obu_header_byte(1, ObuType::Padding, false, true, 0);
        assert!(parse_obu_header(&[header]).is_err());
    }

    #[test]
    fn parse_obu_header_rejects_non_zero_reserved_bit() {
        let header = make_obu_header_byte(0, ObuType::Padding, false, true, 1);
        assert!(parse_obu_header(&[header]).is_err());
    }

    #[test]
    fn parse_obu_header_with_extension_flag_requires_extension_byte() {
        let header = make_obu_header_byte(0, ObuType::FrameHeader, true, true, 0);
        assert!(parse_obu_header(&[header]).is_err());
    }

    #[test]
    fn obu_extension_parses_temporal_and_spatial_ids() {
        let extension = make_obu_extension_byte(7, 3, 0b101);
        let input = [extension, 0xAA];
        let bit_input: BitInput = (&input, 0);
        let ctx = TraceCtx::new(bit_input, 0);

        let (remaining, parsed) = obu_extension(bit_input, ctx).expect("extension should parse");

        assert_eq!(parsed.temporal_id, 7);
        assert_eq!(parsed.spatial_id, 3);
        assert_eq!(remaining.0, &input[1..]);
        assert_eq!(remaining.1, 0);
    }

    #[test]
    fn obu_extension_errors_when_insufficient_bits_are_available() {
        let empty: &[u8] = &[];
        let bit_input: BitInput = (empty, 0);
        let ctx = TraceCtx::new(bit_input, 0);
        assert!(obu_extension(bit_input, ctx).is_err());
    }

    #[test]
    fn obu_type_maps_all_valid_discriminants() {
        for value in 0_u8..=15 {
            let input = [value << 4];
            let expected = ObuType::try_from(value).expect("all 4-bit values map to an obu type");

            let (remaining, parsed) = obu_type((&input, 0)).expect("obu type should parse");

            assert_eq!(parsed, expected);
            assert_eq!(remaining.0, &input);
            assert_eq!(remaining.1, 4);
        }
    }

    #[test]
    fn obu_type_errors_when_insufficient_bits_are_available() {
        assert!(obu_type((&[], 0)).is_err());
    }

    // =================================================================
    // BitstreamParser method tests — helpers and groups 1–8
    // =================================================================

    use super::super::{
        BitstreamParser,
        sequence::{
            ColorConfig, ColorPrimaries, ColorRange, MatrixCoefficients, SequenceHeader,
            TransferCharacteristics,
        },
        util::leb128_write,
    };
    use arrayvec::ArrayVec;

    fn make_parser<const WRITE: bool>(
        size: usize,
        seen_frame_header: bool,
        sequence_header: Option<SequenceHeader>,
        packet_out: Vec<u8>,
    ) -> BitstreamParser<WRITE> {
        BitstreamParser {
            reader: None,
            writer: None,
            packet_out,
            incoming_grain_header: None,
            parsed: false,
            size,
            seen_frame_header,
            sequence_header,
            previous_frame_header: None,
            ref_frame_idx: Default::default(),
            ref_order_hint: Default::default(),
            big_ref_order_hint: Default::default(),
            big_ref_valid: Default::default(),
            big_order_hints: Default::default(),
            grain_headers: Vec::new(),
            strict_mode: false,
        }
    }

    /// Build a complete OBU byte sequence from its parts.
    ///
    /// `obu_type` and flags determine the header byte. `extension` provides the
    /// optional extension byte. When `has_size_field` is true the payload length
    /// is LEB128-encoded between the header and the payload.
    fn build_obu_bytes(
        obu_type: ObuType,
        extension: Option<ObuExtension>,
        has_size_field: bool,
        payload: &[u8],
    ) -> Vec<u8> {
        let header = make_obu_header_byte(0, obu_type, extension.is_some(), has_size_field, 0);
        let mut buf = vec![header];
        if let Some(ext) = extension {
            buf.push(make_obu_extension_byte(ext.temporal_id, ext.spatial_id, 0));
        }
        if has_size_field {
            buf.extend_from_slice(&leb128_write(payload.len() as u32));
        }
        buf.extend_from_slice(payload);
        buf
    }

    fn sequence_header_with_idc(idc: u16) -> SequenceHeader {
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
            cur_operating_point_idc: idc,
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

    /// Minimal bit-level builder duplicated from the `sequence` test module
    /// (which is private). Only the subset needed for building a reduced
    /// sequence header bitstream.
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

    /// Build a minimal valid reduced-still-picture sequence header payload.
    ///
    /// Profile 0, 8-bit, no grain. The parser will consume this via `bits()`
    /// so it must be byte-aligned (the builder zero-pads).
    fn build_reduced_sequence_header_bytes() -> Vec<u8> {
        let mut b = BitBuilder::default();
        b.push_bits(0, 3); // seq_profile = 0
        b.push_bool(true); // still_picture
        b.push_bool(true); // reduced_still_picture_header
        b.push_bits(4, 5); // seq_level_idx = 4
        b.push_bits(0, 4); // frame_width_bits_minus_1 = 0
        b.push_bits(0, 4); // frame_height_bits_minus_1 = 0
        b.push_bits(0, 1); // max_frame_width_minus_1 = 0
        b.push_bits(0, 1); // max_frame_height_minus_1 = 0
        // Reduced suffix: use_128x128_superblock, enable_filter_intra,
        // enable_intra_edge_filter, enable_superres, enable_cdef,
        // enable_restoration
        b.push_bool(false); // use_128x128_superblock
        b.push_bool(false); // enable_filter_intra
        b.push_bool(false); // enable_intra_edge_filter
        b.push_bool(false); // enable_superres
        b.push_bool(false); // enable_cdef
        b.push_bool(false); // enable_restoration
        // color_config for profile 0, 8-bit
        b.push_bool(false); // high_bitdepth
        b.push_bool(false); // monochrome
        b.push_bool(false); // color_description_present_flag
        b.push_bool(false); // color_range = limited
        b.push_bits(0, 2); // chroma_sample_position
        b.push_bool(false); // separate_uv_delta_q
        // film_grain_params_present
        b.push_bool(false);
        b.into_bytes()
    }

    // ===== Group 1: adjust_obu_size =====

    #[test]
    fn adjust_obu_size_same_encoding_length() {
        let mut parser = make_parser::<true>(0, false, None, Vec::new());
        // [0xAA] [LEB(10) = 0x0A] [0xBB]
        parser.packet_out = vec![0xAA, 0x0A, 0xBB];
        let original_len = parser.packet_out.len();
        parser.adjust_obu_size(1, 1, 12);
        assert_eq!(parser.packet_out.len(), original_len);
        assert_eq!(parser.packet_out[0], 0xAA);
        assert_eq!(parser.packet_out[1], 12); // 12 < 128, single-byte LEB
        assert_eq!(parser.packet_out[2], 0xBB);
    }

    #[test]
    fn adjust_obu_size_encoding_shrinks() {
        let mut parser = make_parser::<true>(0, false, None, Vec::new());
        // LEB128(128) = [0x80, 0x01] — 2-byte encoding
        let leb_128 = leb128_write(128);
        assert_eq!(leb_128.len(), 2);
        parser.packet_out = vec![0xAA];
        parser.packet_out.extend_from_slice(&leb_128);
        parser.packet_out.push(0xBB);
        let original_len = parser.packet_out.len();

        parser.adjust_obu_size(1, 2, 5);

        // 5 encodes as 1 byte, so buffer shrinks by 1
        assert_eq!(parser.packet_out.len(), original_len - 1);
        assert_eq!(parser.packet_out[0], 0xAA);
        assert_eq!(parser.packet_out[1], 5);
        assert_eq!(parser.packet_out[2], 0xBB);
    }

    #[test]
    fn adjust_obu_size_encoding_grows() {
        let mut parser = make_parser::<true>(0, false, None, Vec::new());
        // Start with 1-byte LEB for value 100
        parser.packet_out = vec![0xAA, 100, 0xBB];
        let original_len = parser.packet_out.len();

        parser.adjust_obu_size(1, 1, 200);

        let leb_200 = leb128_write(200);
        assert_eq!(leb_200.len(), 2);
        // Buffer grows by 1
        assert_eq!(parser.packet_out.len(), original_len + 1);
        assert_eq!(parser.packet_out[0], 0xAA);
        assert_eq!(&parser.packet_out[1..3], leb_200.as_slice());
        assert_eq!(parser.packet_out[3], 0xBB);
    }

    #[test]
    fn adjust_obu_size_at_start_of_buffer() {
        let mut parser = make_parser::<true>(0, false, None, Vec::new());
        parser.packet_out = vec![42, 0xCC, 0xDD];

        parser.adjust_obu_size(0, 1, 99);

        assert_eq!(parser.packet_out[0], 99);
        assert_eq!(parser.packet_out[1], 0xCC);
        assert_eq!(parser.packet_out[2], 0xDD);
    }

    #[test]
    fn adjust_obu_size_preserves_surrounding_bytes() {
        let mut parser = make_parser::<true>(0, false, None, Vec::new());
        parser.packet_out = vec![0x11, 0x22, 50, 0x33, 0x44];

        parser.adjust_obu_size(2, 1, 60);

        assert_eq!(parser.packet_out[0], 0x11);
        assert_eq!(parser.packet_out[1], 0x22);
        assert_eq!(parser.packet_out[2], 60);
        assert_eq!(parser.packet_out[3], 0x33);
        assert_eq!(parser.packet_out[4], 0x44);
    }

    #[test]
    fn adjust_obu_size_zero_value() {
        let mut parser = make_parser::<true>(0, false, None, Vec::new());
        parser.packet_out = vec![10, 0xFF];

        parser.adjust_obu_size(0, 1, 0);

        assert_eq!(parser.packet_out[0], 0x00);
        assert_eq!(parser.packet_out[1], 0xFF);
    }

    #[test]
    fn adjust_obu_size_large_value() {
        let mut parser = make_parser::<true>(0, false, None, Vec::new());
        // Start with 1-byte LEB
        parser.packet_out = vec![0xAA, 1, 0xBB];

        parser.adjust_obu_size(1, 1, 16384);

        let leb_16384 = leb128_write(16384);
        assert_eq!(leb_16384.len(), 3);
        assert_eq!(parser.packet_out.len(), 2 + 3); // 0xAA + 3-byte LEB + 0xBB
        assert_eq!(parser.packet_out[0], 0xAA);
        assert_eq!(&parser.packet_out[1..4], leb_16384.as_slice());
        assert_eq!(parser.packet_out[4], 0xBB);
    }

    // ===== Group 2: parse_obu — TemporalDelimiter dispatch =====

    #[test]
    fn parse_obu_temporal_delimiter_with_size_field_returns_none() {
        let obu = build_obu_bytes(ObuType::TemporalDelimiter, None, true, &[]);
        let mut parser = make_parser::<false>(0, false, None, Vec::new());

        let (remaining, result) = parser.parse_obu(&obu, 0).expect("should parse TD");

        assert!(remaining.is_empty());
        assert!(result.is_none());
    }

    #[test]
    fn parse_obu_temporal_delimiter_clears_seen_frame_header() {
        let obu = build_obu_bytes(ObuType::TemporalDelimiter, None, true, &[]);
        let mut parser = make_parser::<false>(0, true, None, Vec::new());

        let _ = parser.parse_obu(&obu, 0).expect("should parse TD");

        assert!(!parser.seen_frame_header);
    }

    #[test]
    fn parse_obu_temporal_delimiter_advances_input_by_obu_size() {
        let trailing = [0xDE, 0xAD];
        let mut obu = build_obu_bytes(ObuType::TemporalDelimiter, None, true, &[0x00, 0x01]);
        obu.extend_from_slice(&trailing);
        let mut parser = make_parser::<false>(0, false, None, Vec::new());

        let (remaining, _) = parser.parse_obu(&obu, 0).expect("should parse TD");

        assert_eq!(remaining, &trailing);
    }

    #[test]
    fn parse_obu_temporal_delimiter_write_copies_full_obu() {
        let payload = [0x42, 0x43];
        let obu = build_obu_bytes(ObuType::TemporalDelimiter, None, true, &payload);
        let mut parser = make_parser::<true>(0, false, None, Vec::new());

        let _ = parser.parse_obu(&obu, 0).expect("should parse TD");

        // packet_out should contain the header + LEB size + payload
        assert_eq!(parser.packet_out, obu);
    }

    #[test]
    fn parse_obu_temporal_delimiter_read_does_not_write_packet_out() {
        let obu = build_obu_bytes(ObuType::TemporalDelimiter, None, true, &[0x42]);
        let mut parser = make_parser::<false>(0, false, None, Vec::new());

        let _ = parser.parse_obu(&obu, 0).expect("should parse TD");

        assert!(parser.packet_out.is_empty());
    }

    // ===== Group 3: parse_obu — Unknown/catch-all types =====

    #[test]
    fn parse_obu_padding_type_returns_none() {
        let obu = build_obu_bytes(ObuType::Padding, None, true, &[0xFF; 4]);
        let mut parser = make_parser::<false>(0, false, None, Vec::new());

        let (_, result) = parser.parse_obu(&obu, 0).expect("should parse Padding");

        assert!(result.is_none());
    }

    #[test]
    fn parse_obu_metadata_type_returns_none() {
        let obu = build_obu_bytes(ObuType::Metadata, None, true, &[0x01, 0x02]);
        let mut parser = make_parser::<false>(0, false, None, Vec::new());

        let (_, result) = parser.parse_obu(&obu, 0).expect("should parse Metadata");

        assert!(result.is_none());
    }

    #[test]
    fn parse_obu_unknown_type_write_copies_full_obu() {
        let payload = [0xAA, 0xBB, 0xCC];
        let obu = build_obu_bytes(ObuType::Reserved0, None, true, &payload);
        let mut parser = make_parser::<true>(0, false, None, Vec::new());

        let _ = parser.parse_obu(&obu, 0).expect("should parse Reserved0");

        assert_eq!(parser.packet_out, obu);
    }

    #[test]
    fn parse_obu_unknown_type_advances_input_correctly() {
        let trailing = [0xFE, 0xED];
        let mut obu = build_obu_bytes(ObuType::Padding, None, true, &[0x01, 0x02, 0x03]);
        obu.extend_from_slice(&trailing);
        let mut parser = make_parser::<false>(0, false, None, Vec::new());

        let (remaining, _) = parser.parse_obu(&obu, 0).expect("should parse Padding");

        assert_eq!(remaining, &trailing);
    }

    // ===== Group 4: parse_obu — Size field handling =====

    #[test]
    fn parse_obu_without_size_field_uses_parser_size() {
        // has_size_field=false, no extension → obu_size = self.size - 1
        let header = make_obu_header_byte(0, ObuType::Padding, false, false, 0);
        let payload = [0xAA, 0xBB];
        let mut input = vec![header];
        input.extend_from_slice(&payload);
        // parser.size = 1 (header byte) + 2 (payload) = 3
        // obu_size = self.size - 1 - 0(no ext) = 2
        let mut parser = make_parser::<false>(3, false, None, Vec::new());

        let (remaining, result) = parser.parse_obu(&input, 0).expect("should parse");

        assert!(remaining.is_empty());
        assert!(result.is_none());
    }

    #[test]
    fn parse_obu_without_size_field_with_extension_uses_parser_size() {
        // has_size_field=false, with extension → obu_size = self.size - 2
        let ext = ObuExtension {
            temporal_id: 0,
            spatial_id: 0,
        };
        let header = make_obu_header_byte(0, ObuType::Padding, true, false, 0);
        let ext_byte = make_obu_extension_byte(ext.temporal_id, ext.spatial_id, 0);
        let payload = [0xCC, 0xDD, 0xEE];
        let mut input = vec![header, ext_byte];
        input.extend_from_slice(&payload);
        // parser.size = 2 (header+ext) + 3 (payload) = 5
        // obu_size = self.size - 1 - 1(ext) = 3
        let mut parser = make_parser::<false>(5, false, None, Vec::new());

        let (remaining, result) = parser.parse_obu(&input, 0).expect("should parse");

        assert!(remaining.is_empty());
        assert!(result.is_none());
    }

    #[test]
    fn parse_obu_sets_self_size_to_obu_size() {
        let payload = [0x01; 10];
        let obu = build_obu_bytes(ObuType::Padding, None, true, &payload);
        let mut parser = make_parser::<false>(999, false, None, Vec::new());

        let _ = parser.parse_obu(&obu, 0).expect("should parse");

        assert_eq!(parser.size, payload.len());
    }

    // ===== Group 5: parse_obu — Layer filtering =====

    #[test]
    fn parse_obu_layer_filter_skips_when_not_in_temporal_layer() {
        // temporal_id=1 requires bit 1 of op_pt_idc to be set
        // op_pt_idc=0b01 has bit 0 set but not bit 1
        let ext = ObuExtension {
            temporal_id: 1,
            spatial_id: 0,
        };
        let payload = [0xAA, 0xBB];
        let obu = build_obu_bytes(ObuType::Padding, Some(ext), true, &payload);
        let seq = sequence_header_with_idc(0b01);
        let mut parser = make_parser::<false>(0, false, Some(seq), Vec::new());

        let (remaining, result) = parser.parse_obu(&obu, 0).expect("should parse");

        assert!(remaining.is_empty());
        assert!(result.is_none());
    }

    #[test]
    fn parse_obu_layer_filter_skips_when_not_in_spatial_layer() {
        // spatial_id=1 requires bit 9 (8+1) of op_pt_idc
        // op_pt_idc=0b01 has only bit 0 set
        let ext = ObuExtension {
            temporal_id: 0,
            spatial_id: 1,
        };
        let payload = [0xCC];
        let obu = build_obu_bytes(ObuType::Padding, Some(ext), true, &payload);
        let seq = sequence_header_with_idc(0b01);
        let mut parser = make_parser::<false>(0, false, Some(seq), Vec::new());

        let (remaining, result) = parser.parse_obu(&obu, 0).expect("should parse");

        assert!(remaining.is_empty());
        assert!(result.is_none());
    }

    #[test]
    fn parse_obu_layer_filter_passes_when_in_both_layers() {
        // temporal_id=0 → bit 0, spatial_id=0 → bit 8
        // op_pt_idc = 0b1_0000_0001 = 0x101
        let ext = ObuExtension {
            temporal_id: 0,
            spatial_id: 0,
        };
        let payload = [0xDD; 3];
        let obu = build_obu_bytes(ObuType::Padding, Some(ext), true, &payload);
        let seq = sequence_header_with_idc(0x101);
        let mut parser = make_parser::<false>(0, false, Some(seq), Vec::new());

        let (remaining, result) = parser.parse_obu(&obu, 0).expect("should parse");

        assert!(remaining.is_empty());
        // Padding reaches the catch-all arm and returns None
        assert!(result.is_none());
    }

    #[test]
    fn parse_obu_layer_filter_inactive_when_op_pt_idc_zero() {
        let ext = ObuExtension {
            temporal_id: 3,
            spatial_id: 2,
        };
        let payload = [0xEE];
        let obu = build_obu_bytes(ObuType::Padding, Some(ext), true, &payload);
        // idc=0 disables filtering regardless of temporal/spatial ids
        let seq = sequence_header_with_idc(0);
        let mut parser = make_parser::<false>(0, false, Some(seq), Vec::new());

        let (remaining, result) = parser.parse_obu(&obu, 0).expect("should parse");

        assert!(remaining.is_empty());
        assert!(result.is_none());
    }

    #[test]
    fn parse_obu_layer_filter_inactive_when_no_extension() {
        // No extension byte → no filtering even with active idc
        let payload = [0xFF; 2];
        let obu = build_obu_bytes(ObuType::Padding, None, true, &payload);
        let seq = sequence_header_with_idc(0x01);
        let mut parser = make_parser::<false>(0, false, Some(seq), Vec::new());

        let (remaining, result) = parser.parse_obu(&obu, 0).expect("should parse");

        assert!(remaining.is_empty());
        assert!(result.is_none());
    }

    #[test]
    fn parse_obu_layer_filter_inactive_when_no_sequence_header() {
        let ext = ObuExtension {
            temporal_id: 1,
            spatial_id: 1,
        };
        let payload = [0x11];
        let obu = build_obu_bytes(ObuType::Padding, Some(ext), true, &payload);
        // No sequence header → filtering guard short-circuits
        let mut parser = make_parser::<false>(0, false, None, Vec::new());

        let (remaining, result) = parser.parse_obu(&obu, 0).expect("should parse");

        assert!(remaining.is_empty());
        assert!(result.is_none());
    }

    #[test]
    fn parse_obu_layer_filter_inactive_for_temporal_delimiter() {
        // TemporalDelimiter is exempt from layer filtering
        let ext = ObuExtension {
            temporal_id: 1,
            spatial_id: 1,
        };
        let payload = [0x00];
        let obu = build_obu_bytes(ObuType::TemporalDelimiter, Some(ext), true, &payload);
        // idc that would filter out temporal_id=1, spatial_id=1
        let seq = sequence_header_with_idc(0b01);
        let mut parser = make_parser::<false>(0, true, Some(seq), Vec::new());

        let (remaining, result) = parser.parse_obu(&obu, 0).expect("should parse TD");

        assert!(remaining.is_empty());
        assert!(result.is_none());
        // TD also clears seen_frame_header
        assert!(!parser.seen_frame_header);
    }

    #[test]
    fn parse_obu_layer_filter_skip_write_copies_payload() {
        let ext = ObuExtension {
            temporal_id: 1,
            spatial_id: 0,
        };
        let payload = [0xAA, 0xBB];
        let obu = build_obu_bytes(ObuType::Padding, Some(ext), true, &payload);
        let seq = sequence_header_with_idc(0b01); // bit 1 not set → skip
        let mut parser = make_parser::<true>(0, false, Some(seq), Vec::new());

        let _ = parser.parse_obu(&obu, 0).expect("should parse");

        // WRITE mode copies the full OBU (header + ext + size + payload)
        assert_eq!(parser.packet_out, obu);
    }

    #[test]
    fn parse_obu_layer_filter_skip_advances_input() {
        let ext = ObuExtension {
            temporal_id: 1,
            spatial_id: 0,
        };
        let payload = [0xAA];
        let trailing = [0xFE, 0xED];
        let mut obu = build_obu_bytes(ObuType::Padding, Some(ext), true, &payload);
        obu.extend_from_slice(&trailing);
        let seq = sequence_header_with_idc(0b01);
        let mut parser = make_parser::<false>(0, false, Some(seq), Vec::new());

        let (remaining, _) = parser.parse_obu(&obu, 0).expect("should parse");

        assert_eq!(remaining, &trailing);
    }

    // ===== Group 6: parse_obu — Write path header copying =====

    #[test]
    fn parse_obu_write_copies_one_byte_header() {
        let payload = [0x42; 2];
        let obu = build_obu_bytes(ObuType::Padding, None, true, &payload);
        let mut parser = make_parser::<true>(0, false, None, Vec::new());

        let _ = parser.parse_obu(&obu, 0).expect("should parse");

        // First byte is the header, then LEB size, then payload
        assert_eq!(parser.packet_out[0], obu[0]);
        assert_eq!(parser.packet_out, obu);
    }

    #[test]
    fn parse_obu_write_copies_two_byte_header_with_extension() {
        let ext = ObuExtension {
            temporal_id: 0,
            spatial_id: 0,
        };
        let payload = [0x55];
        let obu = build_obu_bytes(ObuType::Padding, Some(ext), true, &payload);
        // idc=0 so no filtering
        let seq = sequence_header_with_idc(0);
        let mut parser = make_parser::<true>(0, false, Some(seq), Vec::new());

        let _ = parser.parse_obu(&obu, 0).expect("should parse");

        // First 2 bytes are header + extension
        assert_eq!(&parser.packet_out[..2], &obu[..2]);
        assert_eq!(parser.packet_out, obu);
    }

    // ===== Group 7: parse_obu — SequenceHeader dispatch =====

    #[test]
    fn parse_obu_sequence_header_returns_obu_sequence_header() {
        let seq_payload = build_reduced_sequence_header_bytes();
        let obu = build_obu_bytes(ObuType::SequenceHeader, None, true, &seq_payload);
        let mut parser = make_parser::<false>(0, false, None, Vec::new());

        let (_, result) = parser
            .parse_obu(&obu, 0)
            .expect("should parse seq header OBU");

        assert!(
            matches!(result, Some(Obu::SequenceHeader(_))),
            "expected Obu::SequenceHeader variant"
        );
    }

    #[test]
    fn parse_obu_sequence_header_skips_trailing_alignment_bytes() {
        let seq_payload = build_reduced_sequence_header_bytes();
        // Add extra trailing bytes within the OBU size to simulate alignment padding
        let mut padded_payload = seq_payload;
        padded_payload.extend_from_slice(&[0x00; 4]);
        let trailing = [0xBE, 0xEF];
        let mut obu = build_obu_bytes(ObuType::SequenceHeader, None, true, &padded_payload);
        obu.extend_from_slice(&trailing);
        let mut parser = make_parser::<false>(0, false, None, Vec::new());

        let (remaining, result) = parser.parse_obu(&obu, 0).expect("should parse");

        assert!(matches!(result, Some(Obu::SequenceHeader(_))));
        assert_eq!(remaining, &trailing);
    }

    #[test]
    fn parse_obu_sequence_header_write_populates_packet_out() {
        let seq_payload = build_reduced_sequence_header_bytes();
        let obu = build_obu_bytes(ObuType::SequenceHeader, None, true, &seq_payload);
        let mut parser = make_parser::<true>(0, false, None, Vec::new());

        let _ = parser
            .parse_obu(&obu, 0)
            .expect("should parse seq header OBU");

        assert!(!parser.packet_out.is_empty());
    }

    // ===== Group 8: parse_obu — TileGroup panic + error conditions =====

    #[test]
    #[should_panic(expected = "This should only be called from within a frame OBU")]
    fn parse_obu_tile_group_panics() {
        let obu = build_obu_bytes(ObuType::TileGroup, None, true, &[0x00]);
        let mut parser = make_parser::<false>(0, false, None, Vec::new());

        let _ = parser.parse_obu(&obu, 0);
    }

    #[test]
    fn parse_obu_rejects_empty_input() {
        let mut parser = make_parser::<false>(0, false, None, Vec::new());

        assert!(parser.parse_obu(&[], 0).is_err());
    }

    #[test]
    fn parse_obu_rejects_truncated_leb_size() {
        // Header says has_size_field=true, but no LEB128 bytes follow
        let header = make_obu_header_byte(0, ObuType::Padding, false, true, 0);
        let mut parser = make_parser::<false>(0, false, None, Vec::new());

        assert!(parser.parse_obu(&[header], 0).is_err());
    }
}
