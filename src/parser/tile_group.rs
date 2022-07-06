use nom::IResult;

pub fn parse_tile_group_obu<'a>(
    input: &'a [u8],
    seen_frame_header: &'a mut bool,
) -> IResult<&'a [u8], ()> {
    todo!()
}
