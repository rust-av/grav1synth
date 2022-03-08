//! Generic methods used in the AV1 spec in various places

use nom::{bytes::complete::take, IResult};

#[derive(Clone, Copy)]
pub(super) struct ReadResult<T>
where
    T: Copy,
{
    pub value: T,
    pub bytes_read: usize,
}

/// Unsigned integer represented by a variable number of little-endian bytes.
pub(super) fn leb128(mut input: &[u8]) -> IResult<&[u8], ReadResult<u64>> {
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
