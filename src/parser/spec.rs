//! Generic methods used in the AV1 spec in various places

use nom::{bits::complete as bit_parsers, bytes::complete::take, IResult};

use crate::parser::util::take_bool_bit;

#[derive(Clone, Copy)]
pub(in crate::parser) struct ReadResult<T>
where
    T: Copy,
{
    pub value: T,
    pub bytes_read: usize,
}

/// Unsigned integer represented by a variable number of little-endian bytes.
pub(in crate::parser) fn leb128(mut input: &[u8]) -> IResult<&[u8], ReadResult<u64>> {
    let mut value = 0u64;
    let mut leb128_bytes = 0;
    for i in 0..8u8 {
        let result = take(1usize)(input)?;
        input = result.0;
        let leb128_byte = unsafe {
            // SAFETY: We know this contains 1 byte because `take` would fail otherwise.
            *result.1.get_unchecked(0)
        };
        value |= ((leb128_byte & 0x7f) as u64) << (i * 7);
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

/// Variable length unsigned n-bit number appearing directly in the bitstream.
pub(in crate::parser) fn uvlc(mut input: (&[u8], usize)) -> IResult<(&[u8], usize), u32> {
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
