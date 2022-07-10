use nom::{
    bits::{bits, complete as bit_parsers},
    combinator::map_res,
    IResult,
};
use num_enum::TryFromPrimitive;

use super::{
    frame::{parse_frame_obu, FrameHeader},
    sequence::{parse_sequence_header, SequenceHeader},
    util::{leb128, take_bool_bit, take_zero_bit, BitInput},
};

pub fn parse_obu<'a, 'b>(
    input: &'a [u8],
    size: &'b mut usize,
    seen_frame_header: &'b mut bool,
    sequence_header: Option<&'b SequenceHeader>,
    previous_frame_header: Option<&'b FrameHeader>,
) -> IResult<&'a [u8], Option<Obu>> {
    let (input, obu_header) = parse_obu_header(input)?;
    let (input, obu_size) = if obu_header.has_size_field {
        let (input, result) = leb128(input)?;
        (input, result.value as usize)
    } else {
        debug_assert!(*size > 0);
        (
            input,
            *size - 1 - if obu_header.extension.is_some() { 1 } else { 0 },
        )
    };
    *size = obu_size;

    if obu_header.obu_type != ObuType::SequenceHeader
        && obu_header.obu_type != ObuType::TemporalDelimiter
    {
        if let Some(obu_ext) = obu_header.extension {
            if let Some(sequence_header) = sequence_header {
                let op_pt_idc = sequence_header.cur_operating_point_idc;
                if op_pt_idc != 0 {
                    let in_temporal_layer = (op_pt_idc >> obu_ext.temporal_id) & 1 > 0;
                    let in_spatial_layer = (op_pt_idc >> (obu_ext.spatial_id + 8)) & 1 > 0;
                    if !in_temporal_layer || !in_spatial_layer {
                        return Ok((&input[obu_size..], None));
                    }
                }
            }
        }
    }

    match obu_header.obu_type {
        ObuType::SequenceHeader => {
            let (input, header) = parse_sequence_header(input)?;
            Ok((input, Some(Obu::SequenceHeader(header))))
        }
        ObuType::Frame => {
            let (input, header) = parse_frame_obu(
                input,
                obu_size,
                seen_frame_header,
                sequence_header.unwrap(),
                obu_header,
                previous_frame_header,
            )?;
            Ok((input, header.map(Obu::FrameHeader)))
        }
        ObuType::FrameHeader | ObuType::TileGroup => {
            // I'm adding an assert here explicitly because I'm not sure if the spec
            // actually requires this. I think it does. But it's 681 pages.
            unreachable!("This should only be called from within a frame OBU.");
        }
        ObuType::TemporalDelimiter => {
            *seen_frame_header = false;
            Ok((&input[obu_size..], None))
        }
        _ => Ok((&input[obu_size..], None)),
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

pub fn parse_obu_header(input: &[u8]) -> IResult<&[u8], ObuHeader> {
    let (input, obu_header) = bits(|input| {
        let (input, _forbidden_bit) = take_zero_bit(input)?;
        let (input, obu_type) = obu_type(input)?;
        let (input, extension_flag) = take_bool_bit(input)?;
        let (input, has_size_field) = take_bool_bit(input)?;
        let (input, _reserved_1bit) = take_zero_bit(input)?;

        let (input, extension) = if extension_flag {
            let (input, extension) = obu_extension(input)?;
            (input, Some(extension))
        } else {
            (input, None)
        };

        Ok((input, ObuHeader {
            obu_type,
            has_size_field,
            extension,
        }))
    })(input)?;

    Ok((input, obu_header))
}

fn obu_extension(input: BitInput) -> IResult<BitInput, ObuExtension> {
    let (input, temporal_id) = bit_parsers::take(3usize)(input)?;
    let (input, spatial_id) = bit_parsers::take(2usize)(input)?;
    let (input, _reserved): (_, u8) = bit_parsers::take(3usize)(input)?;
    Ok((input, ObuExtension {
        temporal_id,
        spatial_id,
    }))
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

fn obu_type(input: BitInput) -> IResult<BitInput, ObuType> {
    map_res(bit_parsers::take(4usize), |output: u8| {
        ObuType::try_from(output)
    })(input)
}
