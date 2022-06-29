use std::fmt::Debug;

use nom::{bits::complete as bit_parsers, bytes::complete::take, combinator::map, IResult};

pub fn take_bool_bit(input: (&[u8], usize)) -> IResult<(&[u8], usize), bool> {
    map(bit_parsers::take(1usize), |output: u8| output > 0)(input)
}

pub fn take_zero_bit(input: (&[u8], usize)) -> IResult<(&[u8], usize), ()> {
    take_zero_bits(input, 1)
}

pub fn take_zero_bits(input: (&[u8], usize), bits: usize) -> IResult<(&[u8], usize), ()> {
    map(bit_parsers::tag(0u8, bits), |_| ())(input)
}

pub fn trailing_bits(input: (&[u8], usize), bits: usize) -> IResult<(&[u8], usize), ()> {
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
