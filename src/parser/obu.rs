use log::debug;
use nom::{
    bits::{bits, complete as bit_parsers},
    combinator::map_res,
    error::{context, VerboseError},
    IResult,
};
use num_enum::TryFromPrimitive;

use super::{
    frame::FrameHeader,
    sequence::SequenceHeader,
    util::{leb128, leb128_write, take_bool_bit, take_zero_bit, BitInput},
    BitstreamParser,
};

impl<const WRITE: bool> BitstreamParser<WRITE> {
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::cognitive_complexity)]
    pub fn parse_obu<'a>(
        &mut self,
        input: &'a [u8],
        // Once again, this is in 10,000,000ths of a second
        packet_ts: u64,
    ) -> IResult<&'a [u8], Option<Obu>, VerboseError<&'a [u8]>> {
        debug!("Parsing OBU from remaining data of {} bytes", input.len());
        let pre_input = input;
        let packet_start_len = self.packet_out.len();
        let (input, obu_header) = context("Failed parsing obu header", parse_obu_header)(input)?;
        let obu_header_size = if obu_header.extension.is_some() { 2 } else { 1 };
        let obu_size_pos = packet_start_len + obu_header_size;
        let mut leb_size = 0;
        let (input, obu_size) = if obu_header.has_size_field {
            let (input, result) = context("Failed parsing obu size", leb128)(input)?;
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
        debug!("Parsing contents of OBU of size {}", obu_size);
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
        }

        if obu_header.obu_type != ObuType::SequenceHeader
            && obu_header.obu_type != ObuType::TemporalDelimiter
        {
            if let Some(ref obu_ext) = obu_header.extension {
                if let Some(ref sequence_header) = self.sequence_header {
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
            }
        }

        match obu_header.obu_type {
            ObuType::SequenceHeader => {
                debug!("Parsing sequence header");
                let pre_len = input.len();
                let (mut input, header) = context("Failed parsing sequence header", |input| {
                    // Writing handled within this function
                    self.parse_sequence_header(input)
                })(input)?;
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
                debug!("Parsing frame");
                let pre_len = input.len();
                let (mut input, header) = context("Failed parsing frame obu", |input| {
                    // Writing handled within this function
                    self.parse_frame_obu(input, obu_header, packet_ts)
                })(input)?;
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
                debug!("Parsing frame header");
                let pre_len = input.len();
                let (mut input, header) = context("Failed parsing frame header", |input| {
                    // Writing handled within this function
                    self.parse_frame_header(input, obu_header, packet_ts)
                })(input)?;
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
                    self.packet_out.extend(input.iter().take(adjustment));
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

    fn adjust_obu_size(&mut self, pos: usize, leb_size: usize, new_obu_size: usize) {
        let encoded_size = leb128_write(new_obu_size as u32);
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

fn parse_obu_header(input: &[u8]) -> IResult<&[u8], ObuHeader, VerboseError<&[u8]>> {
    let (input, obu_header) = bits(|input| {
        let (input, _forbidden_bit) =
            context("Failed parsing forbidden_bit", take_zero_bit)(input)?;
        let (input, obu_type) = context("Failed parsing obu_type", obu_type)(input)?;
        let (input, extension_flag) =
            context("Failed parsing extension_flag", take_bool_bit)(input)?;
        let (input, has_size_field) =
            context("Failed parsing has_size_field", take_bool_bit)(input)?;
        let (input, _reserved_1bit) =
            context("Failed parsing reserved_1bit", take_zero_bit)(input)?;

        let (input, extension) = if extension_flag {
            let (input, extension) = context("Failed parsing obu extension", obu_extension)(input)?;
            (input, Some(extension))
        } else {
            (input, None)
        };

        Ok((
            input,
            ObuHeader {
                obu_type,
                has_size_field,
                extension,
            },
        ))
    })(input)?;

    Ok((input, obu_header))
}

fn obu_extension(input: BitInput) -> IResult<BitInput, ObuExtension, VerboseError<BitInput>> {
    let (input, temporal_id) = bit_parsers::take(3usize)(input)?;
    let (input, spatial_id) = bit_parsers::take(2usize)(input)?;
    let (input, _reserved): (_, u8) = bit_parsers::take(3usize)(input)?;
    Ok((
        input,
        ObuExtension {
            temporal_id,
            spatial_id,
        },
    ))
}

fn obu_type(input: BitInput) -> IResult<BitInput, ObuType, VerboseError<BitInput>> {
    map_res(bit_parsers::take(4usize), |output: u8| {
        ObuType::try_from(output)
    })(input)
}
