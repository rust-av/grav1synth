use log::debug;
use nom::{IResult, Parser, bits::complete as bit_parsers, error::Error};

use super::util::{self, BitInput, ReadResult};

/// Tracks the bit position context for trace logging.
///
/// RATIONALE: Created at the entry of each `bits()` closure to anchor
/// bit-position calculations. `base_offset` accounts for bits consumed
/// before this closure (OBU header + size bytes).
#[derive(Clone, Copy)]
pub struct TraceCtx<'a> {
    start: BitInput<'a>,
    base_offset: usize,
}

impl<'a> TraceCtx<'a> {
    #[must_use]
    pub fn new(start: BitInput<'a>, base_offset: usize) -> Self {
        Self { start, base_offset }
    }

    /// Computes the absolute bit position of `current` relative to OBU start.
    #[must_use]
    pub fn pos(&self, current: BitInput) -> usize {
        let start_remaining = self.start.0.len() * 8 - self.start.1;
        let current_remaining = current.0.len() * 8 - current.1;
        self.base_offset + start_remaining - current_remaining
    }
}

/// Logs an OBU section header (e.g., "Sequence Header", "Frame Header").
pub fn trace_section(name: &str) {
    debug!(target: "trace_headers", "{name}");
}

/// Logs an unsigned field in FFmpeg `trace_headers` format.
///
/// Format: `<pos left-12><name + binary right-padded to 60 cols> = <value>`
pub fn trace_field(pos: usize, name: &str, num_bits: usize, value: u64) {
    if log::log_enabled!(target: "trace_headers", log::Level::Debug) {
        let bits_str = format!("{value:0>num_bits$b}");
        let pad = 60usize.saturating_sub(name.len());
        debug!(
            target: "trace_headers",
            "{pos:<12}{name}{bits_str:>pad$} = {value}"
        );
    }
}

/// Logs a signed field in FFmpeg `trace_headers` format.
///
/// `raw_bits` is the unsigned interpretation of the two's-complement encoding.
pub fn trace_field_signed(pos: usize, name: &str, num_bits: usize, raw_bits: u64, value: i64) {
    if log::log_enabled!(target: "trace_headers", log::Level::Debug) {
        let bits_str = format!("{raw_bits:0>num_bits$b}");
        let pad = 60usize.saturating_sub(name.len());
        debug!(
            target: "trace_headers",
            "{pos:<12}{name}{bits_str:>pad$} = {value}"
        );
    }
}

// ---------------------------------------------------------------------------
// Parsing + logging wrappers
// ---------------------------------------------------------------------------

/// Parses a single bit as `bool` and logs it.
pub fn trace_bool<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
    name: &str,
) -> IResult<BitInput<'a>, bool, Error<BitInput<'a>>> {
    let pos = ctx.pos(input);
    let (input, value) = util::take_bool_bit(input)?;
    trace_field(pos, name, 1, u64::from(value));
    Ok((input, value))
}

/// Consumes one bit, requiring it to be `0`, and logs it.
pub fn trace_zero_bit<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
    name: &str,
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
    let pos = ctx.pos(input);
    let (input, ()) = util::take_zero_bit(input)?;
    trace_field(pos, name, 1, 0);
    Ok((input, ()))
}

/// Reads `n` bits as `u8` and logs the field.
pub fn trace_take_u8<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
    n: usize,
    name: &str,
) -> IResult<BitInput<'a>, u8, Error<BitInput<'a>>> {
    let pos = ctx.pos(input);
    let (input, value): (_, u8) = bit_parsers::take(n).parse(input)?;
    trace_field(pos, name, n, u64::from(value));
    Ok((input, value))
}

/// Reads `n` bits as `u16` and logs the field.
pub fn trace_take_u16<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
    n: usize,
    name: &str,
) -> IResult<BitInput<'a>, u16, Error<BitInput<'a>>> {
    let pos = ctx.pos(input);
    let (input, value): (_, u16) = bit_parsers::take(n).parse(input)?;
    trace_field(pos, name, n, u64::from(value));
    Ok((input, value))
}

/// Reads `n` bits as `u32` and logs the field.
pub fn trace_take_u32<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
    n: usize,
    name: &str,
) -> IResult<BitInput<'a>, u32, Error<BitInput<'a>>> {
    let pos = ctx.pos(input);
    let (input, value): (_, u32) = bit_parsers::take(n).parse(input)?;
    trace_field(pos, name, n, u64::from(value));
    Ok((input, value))
}

/// Reads `n` bits as `u64` and logs the field.
pub fn trace_take_u64<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
    n: usize,
    name: &str,
) -> IResult<BitInput<'a>, u64, Error<BitInput<'a>>> {
    let pos = ctx.pos(input);
    let (input, value): (_, u64) = bit_parsers::take(n).parse(input)?;
    trace_field(pos, name, n, value);
    Ok((input, value))
}

/// Reads `n` bits as `usize` and logs the field.
pub fn trace_take_usize<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
    n: usize,
    name: &str,
) -> IResult<BitInput<'a>, usize, Error<BitInput<'a>>> {
    let pos = ctx.pos(input);
    let (input, value): (_, usize) = bit_parsers::take(n).parse(input)?;
    trace_field(pos, name, n, value as u64);
    Ok((input, value))
}

/// Parses AV1 `su(n)` (signed n-bit value) and logs it.
pub fn trace_su<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
    n: usize,
    name: &str,
) -> IResult<BitInput<'a>, i64, Error<BitInput<'a>>> {
    let pos = ctx.pos(input);
    let (input, value) = util::su(input, n)?;
    let raw = (value as u64) & ((1u64 << n) - 1);
    trace_field_signed(pos, name, n, raw, value);
    Ok((input, value))
}

/// Consumes zero-valued padding bits one at a time until byte-aligned,
/// logging each bit individually to match FFmpeg's `trace_headers` format.
pub fn trace_byte_alignment<'a>(
    mut input: BitInput<'a>,
    ctx: TraceCtx,
) -> IResult<BitInput<'a>, (), Error<BitInput<'a>>> {
    while input.1 != 0 {
        (input, _) = trace_zero_bit(input, ctx, "zero_bit")?;
    }
    Ok((input, ()))
}

/// Decodes a byte-aligned LEB128 value and logs it.
///
/// `bit_offset` is the absolute bit position of the first LEB128 byte
/// within the current OBU.
pub fn trace_leb128<'a>(
    input: &'a [u8],
    bit_offset: usize,
    name: &str,
) -> IResult<&'a [u8], ReadResult<u64>, Error<&'a [u8]>> {
    let (input, result) = util::leb128(input)?;
    let num_bits = result.bytes_read * 8;
    trace_field(bit_offset, name, num_bits, result.value);
    Ok((input, result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_ctx_pos_at_start_is_base_offset() {
        let data = [0u8; 4];
        let input: BitInput = (&data, 0);
        let ctx = TraceCtx::new(input, 16);
        assert_eq!(ctx.pos(input), 16);
    }

    #[test]
    fn trace_ctx_pos_after_consuming_bits() {
        let data = [0u8; 4];
        let start: BitInput = (&data, 0);
        let ctx = TraceCtx::new(start, 16);
        // Simulate consuming 5 bits: (&data[0..], 5)
        let current: BitInput = (&data, 5);
        assert_eq!(ctx.pos(current), 21);
    }

    #[test]
    fn trace_ctx_pos_after_consuming_across_byte_boundary() {
        let data = [0u8; 4];
        let start: BitInput = (&data, 3);
        let ctx = TraceCtx::new(start, 10);
        // Consumed 5 bits from offset 3: crosses byte boundary → (&data[1..], 0)
        let current: BitInput = (&data[1..], 0);
        assert_eq!(ctx.pos(current), 15);
    }

    #[test]
    fn trace_ctx_pos_with_nonzero_start_offset() {
        let data = [0u8; 4];
        let start: BitInput = (&data, 6);
        let ctx = TraceCtx::new(start, 0);
        // Consumed 10 bits from (data, 6): → (&data[2..], 0)
        let current: BitInput = (&data[2..], 0);
        assert_eq!(ctx.pos(current), 10);
    }
}
