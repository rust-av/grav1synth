mod sequence;

use nom::{
    bits::complete as bit_parsers,
    bytes,
    bytes::complete::take,
    combinator::{map, map_res},
    sequence::tuple,
    IResult,
};
use num_enum::TryFromPrimitive;

use crate::parser::{
    spec::leb128,
    util::{take_bool_bit, take_zero_bit, take_zero_bits, trailing_bits},
    ParserContext,
};

impl ParserContext {
    pub(in crate::parser) fn parse_obu(
        &mut self,
        input: (&[u8], usize),
        size: usize,
    ) -> IResult<(&[u8], usize), ()> {
        let (input, (obu_type, obu_extension_flag, obu_has_size_field)) = obu_header(input)?;
        let (input, obu_size) = if obu_has_size_field {
            obu_size(input)?
        } else {
            (input, size - 1 - obu_extension_flag.is_some() as usize)
        };

        let cur_operating_point_idc = self.operating_point_idc[self.operating_point];
        if ![ObuType::SequenceHeader, ObuType::TemporalDelimiter].contains(&obu_type)
            && cur_operating_point_idc.any()
        {
            if let Some(ext) = obu_extension_flag {
                let in_temporal_layer = cur_operating_point_idc[ext.temporal_id as usize];
                let in_spatial_layer = cur_operating_point_idc[ext.spatial_id as usize + 8];
                if !in_temporal_layer || !in_spatial_layer {
                    return drop_obu(input, obu_size);
                }
            }
        }

        let input_bits = input.0.len() - input.1;
        let (mut input, _) = match obu_type {
            ObuType::SequenceHeader => self.sequence_header_obu(input)?,
            ObuType::TemporalDelimiter => self.temporal_delimiter_obu(input)?,
            ObuType::FrameHeader | ObuType::RedundantFrameHeader => self.frame_header_obu(input)?,
            ObuType::TileGroup => self.tile_group_obu(input, obu_size)?,
            ObuType::Metadata => self.metadata_obu(input)?,
            ObuType::Frame => self.frame_obu(input, obu_size)?,
            ObuType::TileList => self.tile_list_obu(input)?,
            ObuType::Padding => self.padding_obu(input)?,
            _ => self.reserved_obu(input)?,
        };

        if obu_size > 0
            && ![ObuType::TileGroup, ObuType::TileList, ObuType::Frame].contains(&obu_type)
        {
            let remaining_bits = input.0.len() - input.1;
            let bits_read = input_bits - remaining_bits;
            input = trailing_bits(input, obu_size * 8 - bits_read)?.0;
        }

        Ok((input, ()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, TryFromPrimitive)]
#[repr(u8)]
enum ObuType {
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

fn obu_header(
    input: (&[u8], usize),
) -> IResult<(&[u8], usize), (ObuType, Option<ExtensionHeader>, bool)> {
    let (
        mut input,
        (_obu_forbidden_bit, obu_type, obu_extension_flag, obu_has_size_field, _obu_reserved_1bit),
    ) = tuple((
        take_zero_bit,
        obu_type,
        take_bool_bit,
        take_bool_bit,
        take_zero_bit,
    ))(input)?;
    let extension_header = if obu_extension_flag {
        let (ipt, hdr) = obu_extension_header(input)?;
        input = ipt;
        Some(hdr)
    } else {
        None
    };
    Ok((input, (obu_type, extension_header, obu_has_size_field)))
}

fn obu_type(input: (&[u8], usize)) -> IResult<(&[u8], usize), ObuType> {
    map_res(bit_parsers::take(4usize), |output: u8| {
        ObuType::try_from(output)
    })(input)
}

#[derive(Clone, Copy, Debug)]
struct ExtensionHeader {
    temporal_id: u8,
    spatial_id: u8,
}

fn obu_extension_header(input: (&[u8], usize)) -> IResult<(&[u8], usize), ExtensionHeader> {
    map(
        tuple((
            bit_parsers::take(3usize),
            bit_parsers::take(2usize),
            extension_header_reserved_3bits,
        )),
        |(temporal_id, spatial_id, _extension_header_reserved_3bits)| ExtensionHeader {
            temporal_id,
            spatial_id,
        },
    )(input)
}

fn extension_header_reserved_3bits(input: (&[u8], usize)) -> IResult<(&[u8], usize), ()> {
    take_zero_bits(input, 3)
}

fn obu_size(input: (&[u8], usize)) -> IResult<(&[u8], usize), usize> {
    bytes(leb128)(input).map(|(input, res)| (input, res.value as usize))
}

fn drop_obu(input: (&[u8], usize), size: usize) -> IResult<(&[u8], usize), ()> {
    let (input, _) = bytes(take(size))(input)?;
    Ok((input, ()))
}
