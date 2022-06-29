use nom::{
    bits::{bits, complete as bit_parsers},
    combinator::map_res,
    IResult,
};
use num_enum::TryFromPrimitive;

use super::{
    frame::{parse_frame_header, FrameHeader},
    sequence::{parse_sequence_header, SequenceHeader},
    util::{leb128, take_bool_bit, take_zero_bit, trailing_bits},
};

pub fn parse_obu(input: &[u8], size: Option<usize>) -> IResult<&[u8], Option<Obu>> {
    let (input, obu_header) = parse_obu_header(input)?;
    let (input, obu_size) = if obu_header.has_size_field {
        let (input, result) = leb128(input)?;
        (input, result.value as usize)
    } else {
        (
            input,
            size.expect("OBU requires size but no size provided")
                - 1
                - if obu_header.extension_flag { 1 } else { 0 },
        )
    };

    match obu_header.obu_type {
        ObuType::SequenceHeader => {
            let (input, header) = parse_sequence_header(input, obu_size)?;
            Ok((input, Some(Obu::SequenceHeader(header))))
        }
        ObuType::FrameHeader => {
            let (input, header) = parse_frame_header(input, obu_size)?;
            Ok((input, Some(Obu::FrameHeader(header))))
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
    pub extension_flag: bool,
}

pub fn parse_obu_header(input: &[u8]) -> IResult<&[u8], ObuHeader> {
    let (input, obu_header) = bits(|input| {
        let (input, _forbidden_bit) = take_zero_bit(input)?;
        let (input, obu_type) = obu_type(input)?;
        let (input, extension_flag) = take_bool_bit(input)?;
        let (input, has_size_field) = take_bool_bit(input)?;
        let (input, _reserved_1bit) = take_zero_bit(input)?;

        let (input, _) = if extension_flag {
            // Extension flag is 8 bits, it's useless for us, seek forward
            trailing_bits(input, 8)?
        } else {
            (input, ())
        };

        Ok((input, ObuHeader {
            obu_type,
            has_size_field,
            extension_flag,
        }))
    })(input)?;

    Ok((input, obu_header))
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

fn obu_type(input: (&[u8], usize)) -> IResult<(&[u8], usize), ObuType> {
    map_res(bit_parsers::take(4usize), |output: u8| {
        ObuType::try_from(output)
    })(input)
}
