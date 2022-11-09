use std::fmt::Debug;

use arrayvec::ArrayVec;
use nom::{
    bits::complete as bit_parsers, bytes::complete::take, combinator::map, error::VerboseError,
    IResult,
};
use num_traits::PrimInt;

pub type BitInput<'a> = (&'a [u8], usize);

pub fn take_bool_bit(input: BitInput) -> IResult<BitInput, bool, VerboseError<BitInput>> {
    map(bit_parsers::take(1usize), |output: u8| output > 0)(input)
}

pub fn take_zero_bit(input: BitInput) -> IResult<BitInput, (), VerboseError<BitInput>> {
    take_zero_bits(input, 1)
}

pub fn take_zero_bits(
    input: BitInput,
    bits: usize,
) -> IResult<BitInput, (), VerboseError<BitInput>> {
    map(bit_parsers::tag(0u8, bits), |_| ())(input)
}

#[derive(Debug, Clone, Copy)]
pub struct ReadResult<T>
where
    T: Copy + Debug,
{
    pub value: T,
    pub bytes_read: usize,
}

/// Unsigned integer represented by a variable number of little-endian bytes.
pub fn leb128(mut input: &[u8]) -> IResult<&[u8], ReadResult<u64>, VerboseError<&[u8]>> {
    let mut value = 0u64;
    let mut leb128_bytes = 0;
    for i in 0..8u8 {
        let result = take(1usize)(input)?;
        input = result.0;
        // SAFETY: We know this contains 1 byte because `take` would fail otherwise.
        let leb128_byte = unsafe { *result.1.get_unchecked(0) };
        value |= u64::from(leb128_byte & 0x7f) << (i * 7);
        leb128_bytes += 1;
        if (leb128_byte & 0x80) == 0 {
            break;
        }
    }
    Ok((
        input,
        ReadResult {
            value,
            bytes_read: leb128_bytes,
        },
    ))
}

/// Unsigned integer represented by a variable number of little-endian bytes.
///
/// NOTE from libaom:
/// Disallow values larger than 32-bits to ensure consistent behavior on 32 and
/// 64 bit targets: value is typically used to determine buffer allocation size
/// when decoded.
#[must_use]
pub fn leb128_write(value: u32) -> ArrayVec<u8, 8> {
    let mut coded_value = ArrayVec::new();

    let mut value = value;
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7u8;
        if value != 0 {
            // Signal that more bytes follow.
            byte |= 0x80;
        }
        coded_value.push(byte);

        if value == 0 {
            // We have to break at the end of the loop
            // because there must be at least one byte written.
            break;
        }
    }

    coded_value
}

/// Variable length unsigned n-bit number appearing directly in the bitstream.
pub fn uvlc(mut input: BitInput) -> IResult<BitInput, u32, VerboseError<BitInput>> {
    let mut leading_zeros = 0usize;
    loop {
        let (rem, done) = take_bool_bit(input)?;
        input = rem;
        if done {
            break;
        }
        leading_zeros += 1;
    }

    if leading_zeros >= 32 {
        return Ok((input, u32::MAX));
    }
    let (input, value): (_, u32) = bit_parsers::take(leading_zeros)(input)?;
    Ok((input, value + (1 << leading_zeros) - 1))
}

/// The abbreviation `ns` stands for non-symmetric. This encoding is
/// non-symmetric because the values are not all coded with the same number of
/// bits.
pub fn ns(input: BitInput, n: usize) -> IResult<BitInput, u64, VerboseError<BitInput>> {
    // I don't know what these variables stand for.
    // This is from the AV1 spec pdf.
    let w = floor_log2(n) + 1;
    let m = (1 << w) - n;
    let (input, v): (_, u64) = bit_parsers::take(w - 1)(input)?;
    if v < m as u64 {
        return Ok((input, v));
    }
    let (input, extra_bit): (_, u64) = bit_parsers::take(1usize)(input)?;
    Ok((input, (v << 1u8) - m as u64 + extra_bit))
}

pub fn su(input: BitInput, n: usize) -> IResult<BitInput, i64, VerboseError<BitInput>> {
    let (input, mut value) = bit_parsers::take(n)(input)?;
    let sign_mask = 1 << (n - 1);
    if (value & sign_mask) > 0 {
        value -= 2 * sign_mask;
    }
    Ok((input, value))
}

pub fn floor_log2<T: PrimInt>(mut x: T) -> T {
    let zero = T::from(0u8).unwrap();
    let one = T::from(1u8).unwrap();
    let mut s = zero;
    while x != zero {
        x = x >> 1;
        s = s + one;
    }
    s - one
}

#[cfg(test)]
mod tests {
    use quickcheck_macros::quickcheck;

    use super::{leb128, leb128_write};

    #[quickcheck]
    pub fn validate_leb128_write(val: u32) -> bool {
        let encoded = leb128_write(val);
        let result = leb128(&encoded).unwrap();
        u64::from(val) == result.1.value && result.0.is_empty()
    }
}
