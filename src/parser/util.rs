use std::fmt::Debug;

use arrayvec::ArrayVec;
use nom::{IResult, Parser, bits::complete as bit_parsers, bytes::complete::take, error::Error};
use num_traits::PrimInt;

pub type BitInput<'a> = (&'a [u8], usize);

/// Reads and consumes the next bit from a bit-level input tuple.
///
/// Returns `false` for a `0` bit and `true` for a `1` bit.
///
/// # Errors
/// Returns an error if fewer than 1 bit remains in `input`.
pub fn take_bool_bit(input: BitInput) -> IResult<BitInput, bool, Error<BitInput>> {
    bit_parsers::take(1usize)
        .map(|output: u8| output > 0)
        .parse(input)
}

/// Consumes exactly one bit, requiring that bit to be `0`.
///
/// # Errors
/// Returns an error if the next bit is `1` or if no bit is available.
pub fn take_zero_bit(input: BitInput) -> IResult<BitInput, (), Error<BitInput>> {
    take_zero_bits(input, 1)
}

/// Consumes `bits` bits, requiring all consumed bits to be `0`.
///
/// # Errors
/// Returns an error if any consumed bit is `1` or if `input` does not contain
/// enough bits.
pub fn take_zero_bits(input: BitInput, bits: usize) -> IResult<BitInput, (), Error<BitInput>> {
    bit_parsers::tag(0u8, bits).map(|_| ()).parse(input)
}

#[derive(Debug, Clone, Copy)]
pub struct ReadResult<T>
where
    T: Copy + Debug,
{
    pub value: T,
    pub bytes_read: usize,
}

/// Parses an unsigned LEB128 integer from byte-aligned input.
///
/// This AV1 syntax element uses base-128 little-endian groups: each byte
/// contributes 7 payload bits and a continuation flag in bit 7 (`0x80`).
/// Parsing stops when a byte with continuation flag `0` is encountered, or
/// after 8 bytes have been consumed.
///
/// The returned [`ReadResult`] includes both the decoded value and the number
/// of encoded bytes consumed.
///
/// # Errors
/// Returns an error if the input ends before the next LEB128 byte can be read.
///
/// # Notes
/// AV1 conformance imposes additional constraints (for example, value
/// `<= u32::MAX` and a cleared continuation bit in the 8th byte). This function
/// only decodes bytes and does not enforce those conformance checks.
pub fn leb128(mut input: &[u8]) -> IResult<&[u8], ReadResult<u64>, Error<&[u8]>> {
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

/// Encodes a `u32` as unsigned LEB128 bytes.
///
/// The output uses 7 payload bits per byte and sets the high bit (`0x80`) on
/// all bytes except the last, matching the AV1 `leb128()` syntax element.
///
/// The return type has capacity for 8 bytes (the AV1 maximum), while a `u32`
/// value itself always encodes to at most 5 bytes.
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

/// Parses AV1 `uvlc()`: an unsigned variable-length code from the bitstream.
///
/// The code is encoded as:
/// - `leading_zeros` zero bits,
/// - one terminating `1` bit,
/// - `leading_zeros` payload bits.
///
/// The decoded value is `payload + (1 << leading_zeros) - 1`.
///
/// Per AV1 syntax, if `leading_zeros >= 32`, the decoded value is saturated to
/// `u32::MAX`.
///
/// # Errors
/// Returns an error if the input ends before reading the terminating bit or the
/// required payload bits.
pub fn uvlc(mut input: BitInput) -> IResult<BitInput, u32, Error<BitInput>> {
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

/// Parses AV1 `ns(n)`: a non-symmetric unsigned integer coding.
///
/// This coding represents exactly `n` values in the range `0..n`, while often
/// using fewer bits than a fixed-width `ceil(log2(n))` encoding for
/// non-power-of-two ranges.
///
/// # Parameters
/// - `n`: Number of representable values. Must be greater than `0`.
///
/// # Errors
/// Returns an error if there are not enough bits available to read the encoded
/// value.
///
/// # Panics
/// In debug builds, may panic if `n == 0` due to overflow in [`floor_log2`].
pub fn ns(input: BitInput, n: usize) -> IResult<BitInput, u64, Error<BitInput>> {
    // Names follow the AV1 spec pseudocode for `ns(n)`.
    let w = floor_log2(n) + 1;
    let m = (1 << w) - n;
    let (input, v): (_, u64) = bit_parsers::take(w - 1)(input)?;
    if v < m as u64 {
        return Ok((input, v));
    }
    let (input, extra_bit): (_, u64) = bit_parsers::take(1usize)(input)?;
    Ok((input, (v << 1u8) - m as u64 + extra_bit))
}

/// Parses AV1 `su(n)`: a signed value stored in `n` bits.
///
/// The `n` parsed bits are interpreted as the low `n` bits of a two's-complement
/// signed integer and sign-extended to `i64`.
///
/// # Errors
/// Returns an error if fewer than `n` bits are available.
///
/// # Panics
/// Panics if `n == 0`.
pub fn su(input: BitInput, n: usize) -> IResult<BitInput, i64, Error<BitInput>> {
    let (input, mut value) = bit_parsers::take(n)(input)?;
    let sign_mask = 1 << (n - 1);
    if (value & sign_mask) > 0 {
        value -= 2 * sign_mask;
    }
    Ok((input, value))
}

/// Returns `floor(log2(x))` for a strictly positive integer.
///
/// This is equivalent to the index of the highest set bit in `x`.
///
/// # Panics
/// In debug builds, panics when `x == 0` due to subtraction overflow.
///
/// # Notes
/// This helper is intended for non-zero values only.
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
    use nom::Err;
    use quickcheck_macros::quickcheck;

    use super::{leb128, leb128_write, ns, take_bool_bit, take_zero_bit, take_zero_bits, uvlc};

    #[test]
    fn take_bool_bit_reads_false_and_advances_input() {
        let data = [0b0110_0000u8];
        let (remaining, value) =
            take_bool_bit((&data, 0)).expect("expected first bit to parse as false");

        assert!(!value);
        assert_eq!(remaining, (&data[..], 1));
    }

    #[test]
    fn take_bool_bit_reads_true_and_advances_input() {
        let data = [0b1000_0000u8];
        let (remaining, value) =
            take_bool_bit((&data, 0)).expect("expected first bit to parse as true");

        assert!(value);
        assert_eq!(remaining, (&data[..], 1));
    }

    #[test]
    fn take_bool_bit_returns_error_on_empty_input() {
        assert!(take_bool_bit((&[], 0)).is_err());
    }

    #[test]
    fn take_zero_bit_consumes_one_zero_bit() {
        let data = [0b0111_1111u8];
        let (remaining, ()) = take_zero_bit((&data, 0)).expect("expected leading zero bit");

        assert_eq!(remaining, (&data[..], 1));
    }

    #[test]
    fn take_zero_bit_returns_original_input_on_non_zero_bit() {
        let data = [0b1000_0000u8];
        let input = (&data[..], 0usize);
        let err = take_zero_bit(input).expect_err("expected non-zero bit to fail");

        match err {
            Err::Error(err) | Err::Failure(err) => assert_eq!(err.input, input),
            Err::Incomplete(_) => panic!("did not expect incomplete result"),
        }
    }

    #[test]
    fn take_zero_bits_consumes_multiple_zero_bits() {
        let data = [0b1111_0000u8, 0b1010_1010u8];
        let (remaining, ()) =
            take_zero_bits((&data, 4), 4).expect("expected 4 zero bits starting at offset 4");

        assert_eq!(remaining, (&data[1..], 0));
    }

    #[test]
    fn take_zero_bits_returns_original_input_on_non_zero_value() {
        let data = [0b0001_0000u8];
        let input = (&data[..], 0usize);
        let err = take_zero_bits(input, 4).expect_err("expected non-zero 4-bit sequence to fail");

        match err {
            Err::Error(err) | Err::Failure(err) => assert_eq!(err.input, input),
            Err::Incomplete(_) => panic!("did not expect incomplete result"),
        }
    }

    #[test]
    fn take_zero_bits_returns_error_when_input_is_too_short() {
        let data = [0u8];
        assert!(take_zero_bits((&data, 0), 9).is_err());
    }

    #[test]
    fn uvlc_decodes_zero_when_stop_bit_is_first() {
        let data = [0b1000_0000u8];
        let (remaining, value) = uvlc((&data, 0)).expect("expected single-bit uvlc to decode");

        assert_eq!(value, 0);
        assert_eq!(remaining, (&data[..], 1));
    }

    #[test]
    fn uvlc_decodes_payload_when_leading_zeros_are_present() {
        let data = [0b0001_1010u8];
        let (remaining, value) =
            uvlc((&data, 0)).expect("expected uvlc with 3 leading zeros and payload 0b101");

        assert_eq!(value, 12);
        assert_eq!(remaining, (&data[..], 7));
    }

    #[test]
    fn uvlc_decodes_31_leading_zeros_without_saturating() {
        let data = [0u8, 0u8, 0u8, 0b0000_0001u8, 0u8, 0u8, 0u8, 0u8];
        let (remaining, value) =
            uvlc((&data, 0)).expect("expected uvlc with 31 leading zeros to decode normally");

        assert_eq!(value, (1u32 << 31) - 1);
        assert_eq!(remaining, (&data[7..], 7));
    }

    #[test]
    fn uvlc_saturates_to_u32_max_with_32_leading_zeros() {
        let data = [0u8, 0u8, 0u8, 0u8, 0b1010_1010u8];
        let (remaining, value) =
            uvlc((&data, 0)).expect("expected saturated uvlc for 32 leading zeros");

        assert_eq!(value, u32::MAX);
        assert_eq!(remaining, (&data[4..], 1));
    }

    #[test]
    fn uvlc_saturates_to_u32_max_with_more_than_32_leading_zeros() {
        let data = [0u8, 0u8, 0u8, 0u8, 0b0101_0101u8];
        let (remaining, value) =
            uvlc((&data, 0)).expect("expected saturated uvlc for more than 32 leading zeros");

        assert_eq!(value, u32::MAX);
        assert_eq!(remaining, (&data[4..], 2));
    }

    #[test]
    fn uvlc_returns_error_when_terminator_bit_is_missing() {
        let data = [0u8];
        assert!(uvlc((&data, 0)).is_err());
    }

    #[test]
    fn uvlc_returns_error_when_payload_bits_are_missing() {
        let data = [0b0000_0001u8];
        assert!(uvlc((&data, 5)).is_err());
    }

    #[test]
    fn ns_returns_v_when_v_is_less_than_m() {
        // n = 5 => w = 3, m = 3. Prefix v = 0b10 = 2 (< m), so result is v.
        let data = [0b1000_0000u8];
        let (remaining, value) = ns((&data, 0), 5).expect("expected ns() to return v directly");

        assert_eq!(value, 2);
        assert_eq!(remaining, (&data[..], 2));
    }

    #[test]
    fn ns_reads_extra_bit_when_v_is_not_less_than_m() {
        // n = 5 => w = 3, m = 3. Prefix v = 0b11 = 3 (>= m), so one extra bit is read.
        let data = [0b1110_0000u8];
        let (remaining, value) = ns((&data, 0), 5).expect("expected ns() to read extra bit");

        assert_eq!(value, 4);
        assert_eq!(remaining, (&data[..], 3));
    }

    #[test]
    fn ns_reads_zero_extra_bit_when_v_is_not_less_than_m() {
        // n = 5 => w = 3, m = 3. Prefix v = 3 and extra_bit = 0 should decode to 3.
        let data = [0b1100_0000u8];
        let (remaining, value) =
            ns((&data, 0), 5).expect("expected ns() to decode value using zero extra bit");

        assert_eq!(value, 3);
        assert_eq!(remaining, (&data[..], 3));
    }

    #[test]
    fn ns_for_power_of_two_n_does_not_consume_an_extra_bit() {
        // n = 8 => w = 4, m = 8. Prefix v is 3 bits and is always < m, so no extra bit.
        let data = [0b1111_0000u8];
        let (remaining, value) =
            ns((&data, 0), 8).expect("expected power-of-two ns() to use only w - 1 bits");

        assert_eq!(value, 7);
        assert_eq!(remaining, (&data[..], 3));
    }

    #[test]
    fn ns_with_n_equal_one_returns_zero_without_consuming_bits() {
        // n = 1 => w = 1, m = 1, and the prefix width is zero bits.
        let data = [0b1010_0000u8];
        let input = (&data[..], 3usize);
        let (remaining, value) =
            ns(input, 1).expect("expected ns(1) to decode without reading bits");

        assert_eq!(value, 0);
        assert_eq!(remaining, input);
    }

    #[test]
    fn ns_returns_error_when_prefix_bits_are_missing() {
        // n = 5 requires reading 2 prefix bits. Only one bit remains at offset 7.
        let data = [0u8];
        assert!(ns((&data, 7), 5).is_err());
    }

    #[test]
    fn ns_returns_error_when_extra_bit_is_missing() {
        // n = 5 => need 2 prefix bits and maybe one extra. Offset 6 gives exactly 2 bits (11),
        // so parsing reaches the extra-bit read and must fail.
        let data = [0b0000_0011u8];
        assert!(ns((&data, 6), 5).is_err());
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn ns_panics_when_n_is_zero_in_debug_builds() {
        let data = [0u8];
        _ = ns((&data, 0), 0);
    }

    #[quickcheck]
    pub fn validate_leb128_write(val: u32) -> bool {
        let encoded = leb128_write(val);
        let result = leb128(&encoded).unwrap();
        u64::from(val) == result.1.value && result.0.is_empty()
    }
}
