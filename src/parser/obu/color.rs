use nom::{bits::complete as bit_parsers, combinator::map_res, sequence::tuple, IResult};
use num_enum::TryFromPrimitive;

use crate::parser::util::take_bool_bit;

#[derive(Clone, Copy, Default)]
pub(in crate::parser) struct ColorConfig {
    bit_depth: u8,
    mono_chrome: bool,
    color_primaries: ColorPrimaries,
    transfer_characteristics: TransferCharacteristics,
    matrix_coefficients: MatrixCoefficients,
    color_range: ColorRange,
    subsampling: (u8, u8),
    chroma_sample_position: ChromaSamplePosition,
    separate_uv_delta_q: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, TryFromPrimitive)]
#[repr(u8)]
pub(in crate::parser) enum ColorPrimaries {
    Bt709 = 1,
    Unspecified = 2,
    Bt470m = 4,
    Bt470bg = 5,
    Bt601 = 6,
    Smpte240 = 7,
    Film = 8,
    Bt2020 = 9,
    Xyz = 10,
    Smpte431 = 11,
    Smpte432 = 12,
    Ebu3213 = 22,
}

impl Default for ColorPrimaries {
    fn default() -> Self {
        ColorPrimaries::Unspecified
    }
}

#[derive(Clone, Copy, Debug, PartialEq, TryFromPrimitive)]
#[repr(u8)]
pub(in crate::parser) enum TransferCharacteristics {
    Reserved0 = 0,
    Bt709 = 1,
    Unspecified = 2,
    Reserved3 = 3,
    Bt470m = 4,
    Bt470bg = 5,
    Bt601 = 6,
    Smpte240 = 7,
    Linear = 8,
    Log100 = 9,
    Log100Sqrt10 = 10,
    Iec61966 = 11,
    Bt1361 = 12,
    Srgb = 13,
    Bt2020_10Bit = 14,
    Bt2020_12Bit = 15,
    Smpte2084 = 16,
    Smpte428 = 17,
    Hlg = 18,
}

impl Default for TransferCharacteristics {
    fn default() -> Self {
        TransferCharacteristics::Unspecified
    }
}

#[derive(Clone, Copy, Debug, PartialEq, TryFromPrimitive)]
#[repr(u8)]
pub(in crate::parser) enum MatrixCoefficients {
    Identity = 0,
    Bt709 = 1,
    Unspecified = 2,
    Reserved3 = 3,
    Fcc = 4,
    Bt470bg = 5,
    Bt601 = 6,
    Smpte240 = 7,
    SmpteYCgCo = 8,
    Bt2020Ncl = 9,
    Bt2020Cl = 10,
    Smpte2085 = 11,
    ChromaticityNcl = 12,
    ChromaticityCl = 13,
    ICtCp = 14,
}

impl Default for MatrixCoefficients {
    fn default() -> Self {
        MatrixCoefficients::Unspecified
    }
}

#[derive(Clone, Copy, Debug, PartialEq, TryFromPrimitive)]
#[repr(u8)]
pub(in crate::parser) enum ColorRange {
    Limited = 0,
    Full = 1,
}

impl Default for ColorRange {
    fn default() -> Self {
        ColorRange::Limited
    }
}

#[derive(Clone, Copy, Debug, PartialEq, TryFromPrimitive)]
#[repr(u8)]
pub(in crate::parser) enum ChromaSamplePosition {
    Unknown = 0,
    Vertical = 1,
    Colocated = 2,
    Reserved = 3,
}

impl Default for ChromaSamplePosition {
    fn default() -> Self {
        ChromaSamplePosition::Unknown
    }
}

pub(in crate::parser) fn color_config(
    input: (&[u8], usize),
    seq_profile: u8,
) -> IResult<(&[u8], usize), ColorConfig> {
    let (mut input, high_bitdepth) = take_bool_bit(input)?;
    let bit_depth = if seq_profile == 2 && high_bitdepth {
        let (rem, twelve_bit) = take_bool_bit(input)?;
        input = rem;
        if twelve_bit {
            12
        } else {
            10
        }
    } else if seq_profile <= 2 {
        if high_bitdepth {
            10
        } else {
            8
        }
    } else {
        unreachable!("AV1 spec only implements profiles 0, 1, and 2");
    };

    let mono_chrome = if seq_profile == 1 {
        false
    } else {
        let (rem, mono_chrome) = take_bool_bit(input)?;
        input = rem;
        mono_chrome
    };

    let (input, color_description_present_flag) = take_bool_bit(input)?;
    let (mut input, (color_primaries, transfer_characteristics, matrix_coefficients)) =
        if color_description_present_flag {
            color_description(input)?
        } else {
            (
                input,
                (
                    ColorPrimaries::Unspecified,
                    TransferCharacteristics::Unspecified,
                    MatrixCoefficients::Unspecified,
                ),
            )
        };

    if mono_chrome {
        let (rem, color_range) = color_range(input)?;
        input = rem;
        return Ok((
            input,
            ColorConfig {
                bit_depth,
                mono_chrome,
                color_primaries,
                transfer_characteristics,
                matrix_coefficients,
                color_range,
                subsampling: (1, 1),
                chroma_sample_position: ChromaSamplePosition::Unknown,
                separate_uv_delta_q: false,
            },
        ));
    }

    let (color_range, subsampling, chroma_sample_position) = if color_primaries
        == ColorPrimaries::Bt709
        && transfer_characteristics == TransferCharacteristics::Srgb
        && matrix_coefficients == MatrixCoefficients::Identity
    {
        (ColorRange::Full, (0, 0), ChromaSamplePosition::default())
    } else {
        let (rem, color_range) = color_range(input)?;
        input = rem;
        let subsampling = match seq_profile {
            0 => (1, 1),
            1 => (0, 0),
            _ if bit_depth == 12 => {
                let (rem, ss_x) = take_bool_bit(input)?;
                input = rem;
                let ss_y = if ss_x {
                    let (rem, ss_y) = take_bool_bit(input)?;
                    input = rem;
                    ss_y
                } else {
                    false
                };
                (ss_x as u8, ss_y as u8)
            }
            _ => (1, 0),
        };
        let chroma_sample_position = if subsampling.0 > 0 && subsampling.1 > 0 {
            let (rem, csp) = chroma_sample_position(input)?;
            input = rem;
            csp
        } else {
            ChromaSamplePosition::default()
        };
        (color_range, subsampling, chroma_sample_position)
    };
    let (input, separate_uv_delta_q) = take_bool_bit(input)?;

    Ok((
        input,
        ColorConfig {
            bit_depth,
            mono_chrome,
            color_primaries,
            transfer_characteristics,
            matrix_coefficients,
            color_range,
            subsampling,
            chroma_sample_position,
            separate_uv_delta_q,
        },
    ))
}

fn color_description(
    input: (&[u8], usize),
) -> IResult<(&[u8], usize), (ColorPrimaries, TransferCharacteristics, MatrixCoefficients)> {
    tuple((
        map_res(bit_parsers::take(8usize), |color_primaries: u8| {
            ColorPrimaries::try_from(color_primaries)
        }),
        map_res(bit_parsers::take(8usize), |transfer_characteristics: u8| {
            TransferCharacteristics::try_from(transfer_characteristics)
        }),
        map_res(bit_parsers::take(8usize), |matrix_coefficients: u8| {
            MatrixCoefficients::try_from(matrix_coefficients)
        }),
    ))(input)
}

fn color_range(input: (&[u8], usize)) -> IResult<(&[u8], usize), ColorRange> {
    map_res(bit_parsers::take(1usize), |output: u8| {
        ColorRange::try_from(output)
    })(input)
}

fn chroma_sample_position(input: (&[u8], usize)) -> IResult<(&[u8], usize), ChromaSamplePosition> {
    map_res(bit_parsers::take(2usize), |output: u8| {
        ChromaSamplePosition::try_from(output)
    })(input)
}
