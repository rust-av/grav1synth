mod sequence;

use anyhow::Result;
use bitvec::{array::BitArray, bitarr, order::Lsb0, vec::BitVec, view::BitViewSized, BitArr};
use nom::{
    bits::{bits, complete as bit_parsers},
    bytes,
    bytes::complete::take,
    combinator::{map, map_res},
    sequence::tuple,
    IResult,
};
use num_enum::TryFromPrimitive;

use crate::parser::{
    spec::leb128,
    util::{take_bool_bit, take_zero_bit, take_zero_bits},
};

pub(crate) struct ParserContext {
    bit_position: usize,
    timing_info_present_flag: bool,
    decoder_model_info_present_flag: bool,
    initial_display_delay_present_flag: bool,
    operating_points_cnt_minus_1: u8,
    operating_point: usize,
    operating_point_idc: Vec<BitArr!(for 12)>,
    seq_level_idx: Vec<u8>,
    seq_tier: Vec<u8>,
    decoder_model_present_for_this_op: BitVec,
    initial_display_delay_present_for_this_op: BitVec,
    initial_display_delay_minus_1: Vec<usize>,
}

impl ParserContext {
    /// We should make a new one of these for each keyframe
    pub(crate) fn new() -> Self {
        Self {
            bit_position: 0,
            timing_info_present_flag: false,
            decoder_model_info_present_flag: false,
            initial_display_delay_present_flag: false,
            operating_points_cnt_minus_1: 0,
            operating_point: 0,
            operating_point_idc: Vec::new(),
            seq_level_idx: Vec::new(),
            seq_tier: Vec::new(),
            decoder_model_present_for_this_op: BitVec::new(),
            initial_display_delay_present_for_this_op: BitVec::new(),
            initial_display_delay_minus_1: Vec::new(),
        }
    }

    /// Parses an OBU packet (usually one frame + headers).
    pub(crate) fn full_parse<'a>(
        &'a mut self,
        mut input: (&'a [u8], usize),
        size: usize,
    ) -> IResult<(&[u8], usize), ()> {
        while !input.0.is_empty() {
            input = self.parse_obu(input, size)?.0;
        }
        Ok((input, ()))
    }

    fn parse_obu(&mut self, input: (&[u8], usize), size: usize) -> IResult<(&[u8], usize), ()> {
        let (input, (obu_type, obu_extension_flag, obu_has_size_field)) = obu_header(input)?;
        let (input, obu_size) = if obu_has_size_field {
            obu_size(input)?
        } else {
            (input, size - 1 - obu_extension_flag.is_some() as usize)
        };
        if obu_type != ObuType::SequenceHeader
            && obu_type != ObuType::TemporalDelimiter
            && self.cur_operating_point_idc().any()
        {
            if let Some(ext) = obu_extension_flag {
                let in_temporal_layer = *self
                    .cur_operating_point_idc()
                    .get(ext.temporal_id as usize)
                    .as_deref()
                    .expect("cannot fail--has a known length");
                let in_spatial_later = *self
                    .cur_operating_point_idc()
                    .get(ext.spatial_id as usize + 8)
                    .as_deref()
                    .expect("cannot fail--has a known length");
                if !in_temporal_layer || !in_spatial_later {
                    let (input, _) = drop_obu(input, obu_size)?;
                    return Ok((input, ()));
                }
            }
        }

        let (input, _) = match obu_type {
            ObuType::SequenceHeader => self.sequence_header_obu(input)?,
            ObuType::TemporalDelimiter => self.temporal_delimiter_obu(input)?,
            ObuType::FrameHeader | ObuType::RedundantFrameHeader => self.frame_header_obu(input)?,
            ObuType::TileGroup => self.tile_group_obu(input, obu_size)?,
            ObuType::Metadata => self.metadata_obu(input)?,
            ObuType::Frame => self.frame_obu(input, obu_size)?,
            ObuType::TileList => self.tile_list_obu(input)?,
            ObuType::Padding => self.padding_obu(input)?,
            _ => self.reserved_obu()?,
        };
        let input = self.trailing_bits()?;

        Ok((input, ()))
    }

    fn sequence_header_obu(&mut self, input: (&[u8], usize)) -> IResult<(&[u8], usize), ()> {
        let (input, seq_profile) = seq_profile(input)?;
        let (input, still_picture) = still_picture(input)?;
        let (input, reduced_still_picture_header) = reduced_still_picture_header(input)?;
        if reduced_still_picture_header {
            self.timing_info_present_flag = false;
            self.decoder_model_info_present_flag = false;
            self.initial_display_delay_present_flag = false;
            self.operating_points_cnt_minus_1 = 0;
            self.operating_point_idc.push(Default::default());
            self.seq_level_idx.push(seq_level_idx(input)?);
            self.seq_tier.push(0);
            self.decoder_model_present_for_this_op.push(false);
            self.initial_display_delay_present_for_this_op.push(false);
        } else {
            //
        }
        todo!()
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
    let (mut input, (_, ty, ext_flag, has_size, _)) = tuple((
        obu_forbidden_bit,
        obu_type,
        obu_extension_flag,
        obu_has_size_field,
        obu_reserved_1bit,
    ))(input)?;
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

#[derive(Clone, Copy, Debug)]
struct ExtensionHeader {
    temporal_id: u8,
    spatial_id: u8,
}

fn obu_extension_header(input: (&[u8], usize)) -> IResult<(&[u8], usize), ExtensionHeader> {
    map(
        tuple((temporal_id, spatial_id, extension_header_reserved_3bits)),
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

fn obu_size(input: (&[u8], usize)) -> IResult<(&[u8], usize), usize> {
    bytes(leb128)(input).map(|(input, res)| (input, res.value as usize))
}

fn drop_obu(input: (&[u8], usize), size: usize) -> IResult<(&[u8], usize), ()> {
    map(bytes(take(size)), |_| ())(input)
}
