use std::fmt::Debug;

use nom::{bits::complete as bit_parsers, bytes::complete::take, combinator::map, IResult};

pub type BitInput<'a> = (&'a [u8], usize);

pub fn take_bool_bit(input: BitInput) -> IResult<BitInput, bool> {
    map(bit_parsers::take(1usize), |output: u8| output > 0)(input)
}

pub fn take_zero_bit(input: BitInput) -> IResult<BitInput, ()> {
    take_zero_bits(input, 1)
}

pub fn take_zero_bits(input: BitInput, bits: usize) -> IResult<BitInput, ()> {
    map(bit_parsers::tag(0u8, bits), |_| ())(input)
}

pub fn trailing_bits(input: BitInput, bits: usize) -> IResult<BitInput, ()> {
    let (input, _): (_, u64) = bit_parsers::take(bits)(input)?;
    Ok((input, ()))
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
pub fn leb128(mut input: &[u8]) -> IResult<&[u8], ReadResult<u64>> {
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
    Ok((input, ReadResult {
        value,
        bytes_read: leb128_bytes,
    }))
}

/// Variable length unsigned n-bit number appearing directly in the bitstream.
pub fn uvlc(mut input: BitInput) -> IResult<BitInput, u32> {
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
