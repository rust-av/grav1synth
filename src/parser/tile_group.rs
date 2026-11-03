use log::debug;
use nom::{IResult, bits::bits, error::Error};

use super::{
    BitstreamParser,
    frame::TileInfo,
    trace::{TraceCtx, trace_bool, trace_byte_alignment, trace_take_u32},
};

impl<const WRITE: bool> BitstreamParser<WRITE> {
    /// Parses the tile group OBU header and updates frame-boundary tracking state.
    ///
    /// Only the tile group header fields needed to determine `tg_end` are read here; tile
    /// payload bytes are not parsed. When this tile group reaches the last tile in the
    /// frame, `seen_frame_header` is cleared so the next frame can parse a new header.
    ///
    /// In write mode (`WRITE == true`), the original OBU payload bytes are copied through to
    /// `packet_out` unchanged.
    ///
    /// # Parameters
    /// - `input`: Tile group OBU payload bytes.
    /// - `size`: Payload size in bytes to pass through to output in write mode.
    /// - `tile_info`: Per-frame tiling metadata used to decode tile group header fields.
    ///
    /// # Returns
    /// Returns the remaining input after `size` bytes and `()`, or a `nom` parse error if
    /// the tile group header cannot be decoded.
    ///
    /// # Panics
    /// Panics if `size > input.len()`.
    pub fn parse_tile_group_obu<'a>(
        &mut self,
        input: &'a [u8],
        size: usize,
        tile_info: TileInfo,
        obu_bit_offset: usize,
    ) -> IResult<&'a [u8], (), Error<&'a [u8]>> {
        // Tile group header--we only need to parse this part
        let (_, (num_tiles, tg_end)) = bits(|input| {
            let ctx = TraceCtx::new(input, obu_bit_offset);
            let num_tiles = tile_info.tile_cols * tile_info.tile_rows;
            let (input, tile_start_and_end_present) = if num_tiles > 1 {
                trace_bool(input, ctx, "tile_start_and_end_present_flag")?
            } else {
                (input, false)
            };
            if num_tiles == 1 || !tile_start_and_end_present {
                let input = trace_byte_alignment(input, ctx)?.0;
                Ok((input, (num_tiles, num_tiles - 1)))
            } else {
                let tile_bits = (tile_info.tile_cols_log2 + tile_info.tile_rows_log2) as usize;
                let (input, _tg_start) = trace_take_u32(input, ctx, tile_bits, "tg_start")?;
                let (input, tg_end) = trace_take_u32(input, ctx, tile_bits, "tg_end")?;
                let input = trace_byte_alignment(input, ctx)?.0;
                Ok((input, (num_tiles, tg_end)))
            }
        })(input)?;

        // We only care about this
        if tg_end == num_tiles - 1 {
            self.seen_frame_header = false;
        }

        if WRITE {
            self.packet_out.extend_from_slice(&input[..size]);
            debug!("Copying tile group obu of size {}", size);
        }
        Ok((&input[size..], ()))
    }
}

#[cfg(test)]
mod tests {
    use super::{BitstreamParser, TileInfo};

    fn make_parser<const WRITE: bool>(
        seen_frame_header: bool,
        packet_out: Vec<u8>,
    ) -> BitstreamParser<WRITE> {
        BitstreamParser {
            reader: None,
            writer: None,
            packet_out,
            incoming_grain_header: None,
            parsed: false,
            size: 0,
            seen_frame_header,
            sequence_header: None,
            previous_frame_header: None,
            ref_frame_idx: Default::default(),
            ref_order_hint: Default::default(),
            big_ref_order_hint: Default::default(),
            big_ref_valid: Default::default(),
            big_order_hints: Default::default(),
            grain_headers: Vec::new(),
            strict_mode: false,
        }
    }

    fn tile_info(
        tile_cols: u32,
        tile_rows: u32,
        tile_cols_log2: u32,
        tile_rows_log2: u32,
    ) -> TileInfo {
        TileInfo {
            tile_cols,
            tile_rows,
            tile_cols_log2,
            tile_rows_log2,
        }
    }

    #[test]
    fn parse_tile_group_obu_single_tile_skips_start_end_flag_and_clears_seen_frame_header() {
        let mut parser = make_parser::<false>(true, Vec::new());
        let input: [u8; 0] = [];
        let size = 0;

        let (remaining, ()) = parser
            .parse_tile_group_obu(&input, size, tile_info(1, 1, 0, 0), 0)
            .expect("single-tile tile-group OBU should parse without reading header bits");

        assert_eq!(remaining, &input[size..]);
        assert!(!parser.seen_frame_header);
        assert!(parser.packet_out.is_empty());
    }

    #[test]
    fn parse_tile_group_obu_multi_tile_without_start_end_flag_uses_last_tile_index() {
        let mut parser = make_parser::<false>(true, vec![0x55]);
        let input = [0b0000_0000, 0xAB];
        let size = 1;

        let (remaining, ()) = parser
            .parse_tile_group_obu(&input, size, tile_info(2, 2, 1, 1), 0)
            .expect("multi-tile tile-group with tile_start_and_end_present = false should parse");

        assert_eq!(remaining, &input[size..]);
        assert!(!parser.seen_frame_header);
        assert_eq!(parser.packet_out, vec![0x55]);
    }

    #[test]
    fn parse_tile_group_obu_multi_tile_with_start_end_flag_preserves_seen_frame_header_when_not_last()
     {
        let mut parser = make_parser::<false>(true, vec![0xCC]);
        // Bits: tile_start_and_end_present=1, tg_start=01, tg_end=10.
        let input = [0b1011_0000, 0xDE, 0xAD];
        let size = 2;

        let (remaining, ()) = parser
            .parse_tile_group_obu(&input, size, tile_info(2, 2, 1, 1), 0)
            .expect("multi-tile tile-group with explicit tg_start/tg_end should parse");

        assert_eq!(remaining, &input[size..]);
        assert!(parser.seen_frame_header);
        assert_eq!(parser.packet_out, vec![0xCC]);
    }

    #[test]
    fn parse_tile_group_obu_multi_tile_with_start_end_flag_clears_seen_frame_header_when_last() {
        let mut parser = make_parser::<false>(true, Vec::new());
        // Bits: tile_start_and_end_present=1, tg_start=00, tg_end=11.
        let input = [0b1001_1000, 0xFE];
        let size = 1;

        let (remaining, ()) = parser
            .parse_tile_group_obu(&input, size, tile_info(2, 2, 1, 1), 0)
            .expect("multi-tile tile-group should clear seen_frame_header when tg_end is last");

        assert_eq!(remaining, &input[size..]);
        assert!(!parser.seen_frame_header);
    }

    #[test]
    fn parse_tile_group_obu_multi_tile_errors_on_non_zero_alignment_bits() {
        let mut parser = make_parser::<false>(true, Vec::new());
        // Bits: tile_start_and_end_present=0. Remaining 7 bits contain a non-zero bit.
        // 0b0100_0000 → flag=0, then padding bits = 100_0000 → non-zero.
        let input = [0b0100_0000u8, 0xAA];
        let size = 1;

        let result = parser.parse_tile_group_obu(&input, size, tile_info(2, 2, 1, 1), 0);
        assert!(
            result.is_err(),
            "non-zero alignment padding bits should cause a parse error"
        );
    }

    #[test]
    fn parse_tile_group_obu_write_mode_appends_size_bytes_to_packet_out() {
        let mut parser = make_parser::<true>(true, vec![0xAA]);
        let input = [0x12, 0x34, 0x56];
        let size = 2;

        let (remaining, ()) = parser
            .parse_tile_group_obu(&input, size, tile_info(1, 1, 0, 0), 0)
            .expect("write-mode tile-group parse should succeed");

        assert_eq!(remaining, &input[size..]);
        assert_eq!(parser.packet_out, vec![0xAA, 0x12, 0x34]);
    }
}
