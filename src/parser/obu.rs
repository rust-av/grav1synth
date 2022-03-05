use nom::{
    bits::{bits, complete as bit_parsers},
    combinator::{map, map_res},
    sequence::tuple,
    IResult,
};
use num_enum::TryFromPrimitive;

use crate::parser::util::{take_bool_bit, take_zero_bit, take_zero_bits};

pub(super) fn open_bitstream_unit(input: &[u8], size: usize) -> IResult<&[u8], ()> {
    let (input, (ty, ext_flag, has_size)) = obu_header(input)?;
    let obu_size = if has_size { todo!() } else { todo!() };
}

#[derive(Clone, Copy, Debug, PartialEq, TryFromPrimitive)]
#[repr(u8)]
enum ObuType {
    SequenceHeader = 1,
    TemporalDelimiter = 2,
    FrameHeader = 3,
    TileGroup = 4,
    Metadata = 5,
    Frame = 6,
    RedundantFrameHeader = 7,
    TileList = 8,
    Padding = 15,
}

fn obu_header(input: &[u8]) -> IResult<&[u8], (ObuType, Option<ExtensionHeader>, bool)> {
    let (mut input, (_, ty, ext_flag, has_size, _)) = bits(tuple((
        obu_forbidden_bit,
        obu_type,
        obu_extension_flag,
        obu_has_size_field,
        obu_reserved_1bit,
    )))(input)?;
    let extension_header = if ext_flag {
        let (ipt, hdr) = obu_extension_header(input)?;
        input = ipt;
        Some(hdr)
    } else {
        None
    };
    Ok((input, (ty, extension_header, has_size)))
}

fn obu_forbidden_bit(input: (&[u8], usize)) -> IResult<(&[u8], usize), ()> {
    take_zero_bit(input)
}

fn obu_type(input: (&[u8], usize)) -> IResult<(&[u8], usize), ObuType> {
    map_res(bit_parsers::take(4usize), |output: u8| {
        ObuType::try_from(output)
    })(input)
}

fn obu_extension_flag(input: (&[u8], usize)) -> IResult<(&[u8], usize), bool> {
    take_bool_bit(input)
}

fn obu_has_size_field(input: (&[u8], usize)) -> IResult<(&[u8], usize), bool> {
    take_bool_bit(input)
}

fn obu_reserved_1bit(input: (&[u8], usize)) -> IResult<(&[u8], usize), ()> {
    take_zero_bit(input)
}

struct ExtensionHeader {
    temporal_id: u8,
    spatial_id: u8,
}

fn obu_extension_header(input: &[u8]) -> IResult<&[u8], ExtensionHeader> {
    map(
        bits(tuple((
            temporal_id,
            spatial_id,
            extension_header_reserved_3bits,
        ))),
        |(temporal_id, spatial_id, _)| ExtensionHeader {
            temporal_id,
            spatial_id,
        },
    )(input)
}

fn temporal_id(input: (&[u8], usize)) -> IResult<(&[u8], usize), u8> {
    bit_parsers::take(3usize)(input)
}

fn spatial_id(input: (&[u8], usize)) -> IResult<(&[u8], usize), u8> {
    bit_parsers::take(2usize)(input)
}

fn extension_header_reserved_3bits(input: (&[u8], usize)) -> IResult<(&[u8], usize), ()> {
    take_zero_bits(input, 3)
}
