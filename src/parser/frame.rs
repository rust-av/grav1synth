use nom::IResult;

use super::grain::FilmGrainHeader;

#[derive(Debug, Clone)]
pub struct FrameHeader {
    film_grain_params: FilmGrainHeader,
}

pub fn parse_frame_header(input: &[u8]) -> IResult<&[u8], FrameHeader> {
    todo!()
}
