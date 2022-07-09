use arrayvec::ArrayVec;
use nom::{bits::complete as bit_parsers, IResult};

use super::{
    frame::FrameType,
    util::{take_bool_bit, BitInput},
};
use crate::parser::grain;

/// The max number of luma scaling points for grain synthesis
pub const GS_NUM_Y_POINTS: usize = 14;
/// The max number of scaling points per chroma plane for grain synthesis
pub const GS_NUM_UV_POINTS: usize = 10;
/// The max number of luma coefficients for grain synthesis
pub const GS_NUM_Y_COEFFS: usize = 24;
/// The max number of coefficients per chroma plane for grain synthesis
pub const GS_NUM_UV_COEFFS: usize = 25;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilmGrainHeader {
    Disable,
    CopyRefFrame(usize),
    UpdateGrain(FilmGrainParams),
}

/// Specifies parameters for enabling decoder-side grain synthesis for
/// a segment of video from `start_time` to `end_time`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilmGrainParams {
    /// Random seed used for generating grain
    pub grain_seed: u16,

    /// Values for the cutoffs and scale factors for luma scaling points
    pub scaling_points_y: ArrayVec<[u8; 2], GS_NUM_Y_POINTS>,
    /// Values for the cutoffs and scale factors for Cb scaling points
    pub scaling_points_cb: ArrayVec<[u8; 2], GS_NUM_UV_POINTS>,
    /// Values for the cutoffs and scale factors for Cr scaling points
    pub scaling_points_cr: ArrayVec<[u8; 2], GS_NUM_UV_POINTS>,

    /// Determines the range and quantization step of the standard deviation
    /// of film grain.
    ///
    /// Accepts values between `8..=11`.
    ///
    /// Fun story: This actually does not seem to ever be used anywhere.
    /// So we'll just set it to 8 I guess.
    pub scaling_shift: u8,

    /// A factor specifying how many AR coefficients are provided,
    /// based on the forumla `coeffs_len = (2 * ar_coeff_lag * (ar_coeff_lag +
    /// 1))`.
    ///
    /// Accepts values between `0..=3`.
    pub ar_coeff_lag: u8,
    /// Values for the AR coefficients for luma scaling points
    pub ar_coeffs_y: ArrayVec<i8, GS_NUM_Y_COEFFS>,
    /// Values for the AR coefficients for Cb scaling points
    pub ar_coeffs_cb: ArrayVec<i8, GS_NUM_UV_COEFFS>,
    /// Values for the AR coefficients for Cr scaling points
    pub ar_coeffs_cr: ArrayVec<i8, GS_NUM_UV_COEFFS>,
    /// Shift value: Specifies the range of acceptable AR coefficients
    /// 6: [-2, 2)
    /// 7: [-1, 1)
    /// 8: [-0.5, 0.5)
    /// 9: [-0.25, 0.25)
    pub ar_coeff_shift: u8,
    /// Multiplier to the grain strength of the Cb plane
    pub cb_mult: u8,
    /// Multiplier to the grain strength of the Cb plane inherited from the luma
    /// plane
    pub cb_luma_mult: u8,
    /// A base value for the Cb plane grain
    pub cb_offset: u16,
    /// Multiplier to the grain strength of the Cr plane
    pub cr_mult: u8,
    /// Multiplier to the grain strength of the Cr plane inherited from the luma
    /// plane
    pub cr_luma_mult: u8,
    /// A base value for the Cr plane grain
    pub cr_offset: u16,

    /// Scale chroma grain from luma instead of providing chroma scaling points
    pub chroma_scaling_from_luma: bool,
    /// Specifies how much the Gaussian random numbers should be scaled down
    /// during the grain synthesis process.
    pub grain_scale_shift: u8,

    /// Whether film grain blocks should overlap or not
    pub overlap_flag: bool,

    pub clip_to_restricted_range: bool,
}

#[allow(clippy::fn_params_excessive_bools)]
#[allow(clippy::too_many_lines)]
pub fn film_grain_params(
    input: BitInput,
    film_grain_params_present: bool,
    show_frame: bool,
    showable_frame: bool,
    frame_type: FrameType,
    monochrome: bool,
    subsampling: (u8, u8),
) -> IResult<BitInput, FilmGrainHeader> {
    if !film_grain_params_present || (!show_frame && !showable_frame) {
        return Ok((input, FilmGrainHeader::Disable));
    }

    let (input, apply_grain) = take_bool_bit(input)?;
    if !apply_grain {
        return Ok((input, FilmGrainHeader::Disable));
    }

    let (input, grain_seed) = bit_parsers::take(16usize)(input)?;
    let (input, update_grain) = if frame_type == FrameType::Inter {
        take_bool_bit(input)?
    } else {
        (input, true)
    };
    if !update_grain {
        let (input, film_grain_params_ref_idx) = bit_parsers::take(3usize)(input)?;
        return Ok((
            input,
            FilmGrainHeader::CopyRefFrame(film_grain_params_ref_idx),
        ));
    }

    let (mut input, num_y_points) = bit_parsers::take(4usize)(input)?;
    let mut scaling_points_y: ArrayVec<[u8; 2], GS_NUM_Y_POINTS> = ArrayVec::new();
    for _ in 0u8..num_y_points {
        let (inner_input, point_y_value) = bit_parsers::take(8usize)(input)?;
        let (inner_input, point_y_scaling) = bit_parsers::take(8usize)(inner_input)?;
        scaling_points_y.push([point_y_value, point_y_scaling]);
        input = inner_input;
    }

    let mut scaling_points_cb = ArrayVec::new();
    let mut scaling_points_cr = ArrayVec::new();
    let (input, chroma_scaling_from_luma) = if monochrome {
        (input, false)
    } else {
        take_bool_bit(input)?
    };
    let (input, num_cb_points, num_cr_points) = if monochrome
        || chroma_scaling_from_luma
        || (subsampling.0 == 1 && subsampling.1 == 1 && num_y_points == 0)
    {
        (input, 0u8, 0u8)
    } else {
        let (mut input, num_cb_points) = bit_parsers::take(4usize)(input)?;
        for _ in 0..num_cb_points {
            let (inner_input, point_cb_value) = bit_parsers::take(8usize)(input)?;
            let (inner_input, point_cb_scaling) = bit_parsers::take(8usize)(inner_input)?;
            scaling_points_cb.push([point_cb_value, point_cb_scaling]);
            input = inner_input;
        }

        let (mut input, num_cr_points) = bit_parsers::take(4usize)(input)?;
        for _ in 0..num_cr_points {
            let (inner_input, point_cr_value) = bit_parsers::take(8usize)(input)?;
            let (inner_input, point_cr_scaling) = bit_parsers::take(8usize)(inner_input)?;
            scaling_points_cr.push([point_cr_value, point_cr_scaling]);
            input = inner_input;
        }
        (input, num_cb_points, num_cr_points)
    };

    let (input, _grain_scaling_minus_8): (_, u8) = bit_parsers::take(2usize)(input)?;
    let (mut input, ar_coeff_lag) = bit_parsers::take(2usize)(input)?;
    let mut ar_coeffs_y = ArrayVec::new();
    let mut ar_coeffs_cb = ArrayVec::new();
    let mut ar_coeffs_cr = ArrayVec::new();
    let num_pos_luma = 2 * ar_coeff_lag * (ar_coeff_lag + 1);
    let num_pos_chroma = if num_y_points > 0 {
        for _ in 0..num_pos_luma {
            let (inner_input, coeff_plus_128): (_, i16) = bit_parsers::take(8usize)(input)?;
            ar_coeffs_y.push((coeff_plus_128 - 128) as i8);
            input = inner_input;
        }
        num_pos_luma + 1
    } else {
        num_pos_luma
    };
    if chroma_scaling_from_luma || num_cb_points > 0 {
        for _ in 0..num_pos_chroma {
            let (inner_input, coeff_plus_128): (_, i16) = bit_parsers::take(8usize)(input)?;
            ar_coeffs_cb.push((coeff_plus_128 - 128) as i8);
            input = inner_input;
        }
    }
    if chroma_scaling_from_luma || num_cr_points > 0 {
        for _ in 0..num_pos_chroma {
            let (inner_input, coeff_plus_128): (_, i16) = bit_parsers::take(8usize)(input)?;
            ar_coeffs_cr.push((coeff_plus_128 - 128) as i8);
            input = inner_input;
        }
    }

    let (input, ar_coeff_shift_minus_6): (_, u8) = bit_parsers::take(2usize)(input)?;
    let (input, grain_scale_shift) = bit_parsers::take(2usize)(input)?;
    let (input, cb_mult, cb_luma_mult, cb_offset) = if num_cb_points > 0 {
        let (input, cb_mult) = bit_parsers::take(8usize)(input)?;
        let (input, cb_luma_mult) = bit_parsers::take(8usize)(input)?;
        let (input, cb_offset) = bit_parsers::take(9usize)(input)?;
        (input, cb_mult, cb_luma_mult, cb_offset)
    } else {
        (input, 0, 0, 0)
    };
    let (input, cr_mult, cr_luma_mult, cr_offset) = if num_cr_points > 0 {
        let (input, cr_mult) = bit_parsers::take(8usize)(input)?;
        let (input, cr_luma_mult) = bit_parsers::take(8usize)(input)?;
        let (input, cr_offset) = bit_parsers::take(9usize)(input)?;
        (input, cr_mult, cr_luma_mult, cr_offset)
    } else {
        (input, 0, 0, 0)
    };
    let (input, overlap_flag) = take_bool_bit(input)?;
    let (input, clip_to_restricted_range) = take_bool_bit(input)?;

    Ok((
        input,
        FilmGrainHeader::UpdateGrain(FilmGrainParams {
            grain_seed,
            scaling_points_y,
            scaling_points_cb,
            scaling_points_cr,
            scaling_shift: 8,
            ar_coeff_lag,
            ar_coeffs_y,
            ar_coeffs_cb,
            ar_coeffs_cr,
            ar_coeff_shift: ar_coeff_shift_minus_6 + 6,
            cb_mult,
            cb_luma_mult,
            cb_offset,
            cr_mult,
            cr_luma_mult,
            cr_offset,
            chroma_scaling_from_luma,
            grain_scale_shift,
            overlap_flag,
            clip_to_restricted_range,
        }),
    ))
}
