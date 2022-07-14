use nom::{
    bits::{bits, complete as bit_parsers},
    error::VerboseError,
    IResult,
};

use super::{frame::TileInfo, util::take_bool_bit, BitstreamParser};

impl<const WRITE: bool> BitstreamParser<WRITE> {
    pub fn parse_tile_group_obu<'a>(
        &mut self,
        input: &'a [u8],
        size: usize,
        tile_info: TileInfo,
    ) -> IResult<&'a [u8], (), VerboseError<&'a [u8]>> {
        // Tile group header--we only need to parse this part
        let (_, (num_tiles, tg_end)) = bits(|input| {
            let num_tiles = tile_info.tile_cols * tile_info.tile_rows;
            let (input, tile_start_and_end_present) = if num_tiles > 1 {
                take_bool_bit(input)?
            } else {
                (input, false)
            };
            if num_tiles == 1 || !tile_start_and_end_present {
                Ok((input, (num_tiles, num_tiles - 1)))
            } else {
                let tile_bits = tile_info.tile_cols_log2 + tile_info.tile_rows_log2;
                let (input, _tg_start): (_, u32) = bit_parsers::take(tile_bits)(input)?;
                let (input, tg_end): (_, u32) = bit_parsers::take(tile_bits)(input)?;
                Ok((input, (num_tiles, tg_end)))
            }
        })(input)?;

        // We only care about this
        if tg_end == num_tiles - 1 {
            self.seen_frame_header = false;
        }

        if WRITE {
            self.packet_out.extend_from_slice(&input[..size]);
        }
        Ok((&input[size..], ()))
    }
}
