use nom::IResult;

pub fn parse_tile_group_obu<'a, 'b>(
    input: &'a [u8],
    size: usize,
    seen_frame_header: &'b mut bool,
) -> IResult<&'a [u8], ()> {
    todo!()
}
