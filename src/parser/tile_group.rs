use nom::{
    bits::{bits, complete as bit_parsers},
    IResult,
};

use super::util::take_bool_bit;

pub fn parse_tile_group_obu<'a, 'b>(
    input: &'a [u8],
    size: usize,
    seen_frame_header: &'b mut bool,
    tile_cols: usize,
    tile_rows: usize,
    tile_cols_log2: usize,
    tile_rows_log2: usize,
) -> IResult<&'a [u8], ()> {
    // Tile group header--we only need to parse this part
    let (_, (num_tiles, tg_end)) = bits(|input| {
        let num_tiles = tile_cols * tile_rows;
        let (input, tile_start_and_end_present) = if num_tiles > 1 {
            take_bool_bit(input)?
        } else {
            (input, false)
        };
        if num_tiles == 1 || !tile_start_and_end_present {
            Ok((input, (0, num_tiles - 1)))
        } else {
            let tile_bits = tile_cols_log2 + tile_rows_log2;
            let (input, tg_start): (_, usize) = bit_parsers::take(tile_bits)(input)?;
            let (input, tg_end): (_, usize) = bit_parsers::take(tile_bits)(input)?;
            Ok((input, (num_tiles, tg_end)))
        }
    })(input)?;

    // We only care about this
    if tg_end == num_tiles - 1 {
        *seen_frame_header = false;
    }

    Ok((&input[size..], ()))
}
