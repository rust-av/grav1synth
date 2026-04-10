use arrayvec::ArrayVec;
use av1_grain::{NUM_UV_COEFFS, NUM_UV_POINTS, NUM_Y_COEFFS, NUM_Y_POINTS};
use nom::{IResult, error::Error};

use super::{
    frame::FrameType,
    trace::{TraceCtx, trace_bool, trace_take_u8, trace_take_u16},
    util::BitInput,
};

#[derive(Debug, Clone, PartialEq)]
pub enum FilmGrainHeader {
    Disable,
    CopyRefFrame,
    UpdateGrain(FilmGrainParams),
}

/// Specifies parameters for enabling decoder-side grain synthesis for
/// a segment of video from `start_time` to `end_time`.
#[derive(Debug, Clone)]
pub struct FilmGrainParams {
    /// Random seed used for generating grain
    pub grain_seed: u16,

    /// Values for the cutoffs and scale factors for luma scaling points
    pub scaling_points_y: ArrayVec<[u8; 2], NUM_Y_POINTS>,
    /// Values for the cutoffs and scale factors for Cb scaling points
    pub scaling_points_cb: ArrayVec<[u8; 2], NUM_UV_POINTS>,
    /// Values for the cutoffs and scale factors for Cr scaling points
    pub scaling_points_cr: ArrayVec<[u8; 2], NUM_UV_POINTS>,

    /// Determines the range and quantization step of the standard deviation
    /// of film grain.
    ///
    /// Accepts values between `8..=11`.
    pub scaling_shift: u8,

    /// A factor specifying how many AR coefficients are provided,
    /// based on the forumla `coeffs_len = (2 * ar_coeff_lag * (ar_coeff_lag +
    /// 1))`.
    ///
    /// Accepts values between `0..=3`.
    pub ar_coeff_lag: u8,
    /// Values for the AR coefficients for luma scaling points
    pub ar_coeffs_y: ArrayVec<i8, NUM_Y_COEFFS>,
    /// Values for the AR coefficients for Cb scaling points
    pub ar_coeffs_cb: ArrayVec<i8, NUM_UV_COEFFS>,
    /// Values for the AR coefficients for Cr scaling points
    pub ar_coeffs_cr: ArrayVec<i8, NUM_UV_COEFFS>,
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

impl PartialEq for FilmGrainParams {
    fn eq(&self, other: &Self) -> bool {
        // We do not want to consider grain seed when comparing if these are equal
        self.scaling_points_y == other.scaling_points_y
            && self.scaling_points_cb == other.scaling_points_cb
            && self.scaling_points_cr == other.scaling_points_cr
            && self.scaling_shift == other.scaling_shift
            && self.ar_coeff_lag == other.ar_coeff_lag
            && self.ar_coeffs_y == other.ar_coeffs_y
            && self.ar_coeffs_cb == other.ar_coeffs_cb
            && self.ar_coeffs_cr == other.ar_coeffs_cr
            && self.ar_coeff_shift == other.ar_coeff_shift
            && self.cb_mult == other.cb_mult
            && self.cb_luma_mult == other.cb_luma_mult
            && self.cb_offset == other.cb_offset
            && self.cr_mult == other.cr_mult
            && self.cr_luma_mult == other.cr_luma_mult
            && self.cr_offset == other.cr_offset
            && self.chroma_scaling_from_luma == other.chroma_scaling_from_luma
            && self.grain_scale_shift == other.grain_scale_shift
            && self.overlap_flag == other.overlap_flag
            && self.clip_to_restricted_range == other.clip_to_restricted_range
    }
}

impl From<av1_grain::GrainTableSegment> for FilmGrainParams {
    fn from(data: av1_grain::GrainTableSegment) -> Self {
        FilmGrainParams {
            grain_seed: data.random_seed,
            scaling_points_y: data.scaling_points_y,
            scaling_points_cb: data.scaling_points_cb,
            scaling_points_cr: data.scaling_points_cr,
            scaling_shift: data.scaling_shift,
            ar_coeff_lag: data.ar_coeff_lag,
            ar_coeffs_y: data.ar_coeffs_y,
            ar_coeffs_cb: data.ar_coeffs_cb,
            ar_coeffs_cr: data.ar_coeffs_cr,
            ar_coeff_shift: data.ar_coeff_shift,
            cb_mult: data.cb_mult,
            cb_luma_mult: data.cb_luma_mult,
            cb_offset: data.cb_offset,
            cr_mult: data.cr_mult,
            cr_luma_mult: data.cr_luma_mult,
            cr_offset: data.cr_offset,
            chroma_scaling_from_luma: data.chroma_scaling_from_luma,
            grain_scale_shift: data.grain_scale_shift,
            overlap_flag: data.overlap_flag,
            clip_to_restricted_range: true,
        }
    }
}

#[allow(clippy::too_many_lines)]
pub fn film_grain_params<'a>(
    input: BitInput<'a>,
    ctx: TraceCtx,
    film_grain_allowed: bool,
    frame_type: FrameType,
    monochrome: bool,
    subsampling: (u8, u8),
) -> IResult<BitInput<'a>, FilmGrainHeader, Error<BitInput<'a>>> {
    if !film_grain_allowed {
        return Ok((input, FilmGrainHeader::Disable));
    }

    let (input, apply_grain) = trace_bool(input, ctx, "apply_grain")?;
    if !apply_grain {
        return Ok((input, FilmGrainHeader::Disable));
    }

    let (input, grain_seed) = trace_take_u16(input, ctx, 16, "grain_seed")?;
    let (input, update_grain) = if frame_type == FrameType::Inter {
        trace_bool(input, ctx, "update_grain")?
    } else {
        (input, true)
    };
    if !update_grain {
        let (input, _film_grain_params_ref_idx) =
            trace_take_u8(input, ctx, 3, "film_grain_params_ref_idx")?;
        return Ok((input, FilmGrainHeader::CopyRefFrame));
    }

    let (mut input, num_y_points) = trace_take_u8(input, ctx, 4, "num_y_points")?;
    let mut scaling_points_y: ArrayVec<[u8; 2], NUM_Y_POINTS> = ArrayVec::new();
    for i in 0u8..num_y_points {
        let (inner_input, point_y_value) =
            trace_take_u8(input, ctx, 8, &format!("point_y_value[{i}]"))?;
        let (inner_input, point_y_scaling) =
            trace_take_u8(inner_input, ctx, 8, &format!("point_y_scaling[{i}]"))?;
        scaling_points_y.push([point_y_value, point_y_scaling]);
        input = inner_input;
    }

    let mut scaling_points_cb: ArrayVec<_, NUM_UV_POINTS> = ArrayVec::new();
    let mut scaling_points_cr: ArrayVec<_, NUM_UV_POINTS> = ArrayVec::new();
    let (input, chroma_scaling_from_luma) = if monochrome {
        (input, false)
    } else {
        trace_bool(input, ctx, "chroma_scaling_from_luma")?
    };
    let (input, num_cb_points, num_cr_points) = if monochrome
        || chroma_scaling_from_luma
        || (subsampling.0 == 1 && subsampling.1 == 1 && num_y_points == 0)
    {
        (input, 0u8, 0u8)
    } else {
        let (mut input, num_cb_points) = trace_take_u8(input, ctx, 4, "num_cb_points")?;
        for i in 0..num_cb_points {
            let (inner_input, point_cb_value) =
                trace_take_u8(input, ctx, 8, &format!("point_cb_value[{i}]"))?;
            let (inner_input, point_cb_scaling) =
                trace_take_u8(inner_input, ctx, 8, &format!("point_cb_scaling[{i}]"))?;
            scaling_points_cb.push([point_cb_value, point_cb_scaling]);
            input = inner_input;
        }

        let (mut input, num_cr_points) = trace_take_u8(input, ctx, 4, "num_cr_points")?;
        for i in 0..num_cr_points {
            let (inner_input, point_cr_value) =
                trace_take_u8(input, ctx, 8, &format!("point_cr_value[{i}]"))?;
            let (inner_input, point_cr_scaling) =
                trace_take_u8(inner_input, ctx, 8, &format!("point_cr_scaling[{i}]"))?;
            scaling_points_cr.push([point_cr_value, point_cr_scaling]);
            input = inner_input;
        }
        (input, num_cb_points, num_cr_points)
    };

    let (input, grain_scaling_minus_8) = trace_take_u8(input, ctx, 2, "grain_scaling_minus_8")?;
    let (mut input, ar_coeff_lag) = trace_take_u8(input, ctx, 2, "ar_coeff_lag")?;
    let mut ar_coeffs_y = ArrayVec::new();
    let mut ar_coeffs_cb = ArrayVec::new();
    let mut ar_coeffs_cr = ArrayVec::new();
    let num_pos_luma = 2 * ar_coeff_lag * (ar_coeff_lag + 1);
    let num_pos_chroma = if num_y_points > 0 {
        for i in 0..num_pos_luma {
            let (inner_input, coeff_plus_128): (_, u8) =
                trace_take_u8(input, ctx, 8, &format!("ar_coeffs_y_plus_128[{i}]"))?;
            ar_coeffs_y.push((i16::from(coeff_plus_128) - 128) as i8);
            input = inner_input;
        }
        num_pos_luma + 1
    } else {
        num_pos_luma
    };
    if chroma_scaling_from_luma || num_cb_points > 0 {
        for i in 0..num_pos_chroma {
            let (inner_input, coeff_plus_128): (_, u8) =
                trace_take_u8(input, ctx, 8, &format!("ar_coeffs_cb_plus_128[{i}]"))?;
            ar_coeffs_cb.push((i16::from(coeff_plus_128) - 128) as i8);
            input = inner_input;
        }
    } else {
        ar_coeffs_cb.push(0);
    }
    if chroma_scaling_from_luma || num_cr_points > 0 {
        for i in 0..num_pos_chroma {
            let (inner_input, coeff_plus_128): (_, u8) =
                trace_take_u8(input, ctx, 8, &format!("ar_coeffs_cr_plus_128[{i}]"))?;
            ar_coeffs_cr.push((i16::from(coeff_plus_128) - 128) as i8);
            input = inner_input;
        }
    } else {
        ar_coeffs_cr.push(0);
    }

    let (input, ar_coeff_shift_minus_6) = trace_take_u8(input, ctx, 2, "ar_coeff_shift_minus_6")?;
    let (input, grain_scale_shift) = trace_take_u8(input, ctx, 2, "grain_scale_shift")?;
    let (input, cb_mult, cb_luma_mult, cb_offset) = if num_cb_points > 0 {
        let (input, cb_mult) = trace_take_u8(input, ctx, 8, "cb_mult")?;
        let (input, cb_luma_mult) = trace_take_u8(input, ctx, 8, "cb_luma_mult")?;
        let (input, cb_offset) = trace_take_u16(input, ctx, 9, "cb_offset")?;
        (input, cb_mult, cb_luma_mult, cb_offset)
    } else {
        (input, 0, 0, 0)
    };
    let (input, cr_mult, cr_luma_mult, cr_offset) = if num_cr_points > 0 {
        let (input, cr_mult) = trace_take_u8(input, ctx, 8, "cr_mult")?;
        let (input, cr_luma_mult) = trace_take_u8(input, ctx, 8, "cr_luma_mult")?;
        let (input, cr_offset) = trace_take_u16(input, ctx, 9, "cr_offset")?;
        (input, cr_mult, cr_luma_mult, cr_offset)
    } else {
        (input, 0, 0, 0)
    };
    let (input, overlap_flag) = trace_bool(input, ctx, "overlap_flag")?;
    let (input, clip_to_restricted_range) = trace_bool(input, ctx, "clip_to_restricted_range")?;

    Ok((
        input,
        FilmGrainHeader::UpdateGrain(FilmGrainParams {
            grain_seed,
            scaling_points_y,
            scaling_points_cb,
            scaling_points_cr,
            scaling_shift: grain_scaling_minus_8 + 8,
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

#[cfg(test)]
mod tests {
    use super::super::trace::TraceCtx;
    use super::super::util::BitInput;
    use super::{FilmGrainHeader, FilmGrainParams, FrameType, film_grain_params};

    fn test_ctx(input: BitInput) -> TraceCtx {
        TraceCtx::new(input, 0)
    }

    #[derive(Default)]
    struct BitBuilder {
        bits: Vec<bool>,
    }

    impl BitBuilder {
        fn push_bool(&mut self, bit: bool) {
            self.bits.push(bit);
        }

        fn push_bits(&mut self, value: u64, width: usize) {
            for shift in (0..width).rev() {
                self.bits.push(((value >> shift) & 1) == 1);
            }
        }

        fn push_scaling_point(&mut self, value: u8, scaling: u8) {
            self.push_bits(u64::from(value), 8);
            self.push_bits(u64::from(scaling), 8);
        }

        fn push_signed_coeff(&mut self, coeff: i8) {
            let encoded = u8::try_from(i16::from(coeff) + 128)
                .expect("film grain AR coefficient must fit encoded u8 range");
            self.push_bits(u64::from(encoded), 8);
        }

        fn len(&self) -> usize {
            self.bits.len()
        }

        fn into_bytes(self) -> Vec<u8> {
            let mut bytes = vec![0u8; self.bits.len().div_ceil(8)];
            for (idx, bit) in self.bits.into_iter().enumerate() {
                if bit {
                    bytes[idx / 8] |= 1u8 << (7 - (idx % 8));
                }
            }
            bytes
        }
    }

    fn with_trailer(mut bits: BitBuilder) -> (Vec<u8>, usize) {
        let consumed_bits = bits.len();
        bits.push_bits(0b1010_0101, 8);
        (bits.into_bytes(), consumed_bits)
    }

    fn assert_remaining_position(remaining: (&[u8], usize), input: &[u8], consumed_bits: usize) {
        assert_eq!(remaining.0, &input[consumed_bits / 8..]);
        assert_eq!(remaining.1, consumed_bits % 8);
    }

    fn expect_update_params(header: FilmGrainHeader) -> FilmGrainParams {
        match header {
            FilmGrainHeader::UpdateGrain(params) => params,
            other => panic!("expected UpdateGrain header, got {other:?}"),
        }
    }

    #[test]
    fn film_grain_params_returns_disable_without_consuming_when_not_allowed() {
        let data = [0b1010_0000u8];
        let input = (&data[..], 3usize);

        let (remaining, parsed) = film_grain_params(
            input,
            test_ctx(input),
            false,
            FrameType::Inter,
            false,
            (0, 0),
        )
        .expect("expected parser to return Disable when film grain is disallowed");

        assert_eq!(parsed, FilmGrainHeader::Disable);
        assert_eq!(remaining, input);
    }

    #[test]
    fn film_grain_params_returns_disable_when_apply_grain_is_false() {
        let mut bits = BitBuilder::default();
        bits.push_bool(false);

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) = film_grain_params(
            input,
            test_ctx(input),
            true,
            FrameType::Inter,
            false,
            (0, 0),
        )
        .expect("expected parser to read apply_grain and disable film grain");

        assert_eq!(parsed, FilmGrainHeader::Disable);
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn film_grain_params_returns_copy_ref_frame_when_inter_frame_disables_update() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true);
        bits.push_bits(0x1234, 16);
        bits.push_bool(false);
        bits.push_bits(0b101, 3);

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) = film_grain_params(
            input,
            test_ctx(input),
            true,
            FrameType::Inter,
            false,
            (0, 0),
        )
        .expect("expected parser to parse inter-frame copy-from-reference mode");

        assert_eq!(parsed, FilmGrainHeader::CopyRefFrame);
        assert_remaining_position(remaining, &data, consumed_bits);
    }

    #[test]
    fn film_grain_params_key_frame_forces_update_and_uses_420_zero_luma_shortcut() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true);
        bits.push_bits(0xBEEF, 16);
        bits.push_bits(0, 4);
        bits.push_bool(false);
        bits.push_bits(0b10, 2);
        bits.push_bits(0, 2);
        bits.push_bits(0b01, 2);
        bits.push_bits(0b10, 2);
        bits.push_bool(true);
        bits.push_bool(false);

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) =
            film_grain_params(input, test_ctx(input), true, FrameType::Key, false, (1, 1))
                .expect("expected parser to parse key-frame update-grain payload");

        assert_remaining_position(remaining, &data, consumed_bits);
        let params = expect_update_params(parsed);
        assert_eq!(params.grain_seed, 0xBEEF);
        assert!(params.scaling_points_y.is_empty());
        assert!(!params.chroma_scaling_from_luma);
        assert!(params.scaling_points_cb.is_empty());
        assert!(params.scaling_points_cr.is_empty());
        assert_eq!(params.ar_coeff_lag, 0);
        assert_eq!(params.ar_coeffs_y.as_slice(), &[]);
        assert_eq!(params.ar_coeffs_cb.as_slice(), &[0]);
        assert_eq!(params.ar_coeffs_cr.as_slice(), &[0]);
        assert_eq!(params.scaling_shift, 8);
        assert_eq!(params.ar_coeff_shift, 7);
        assert_eq!(params.grain_scale_shift, 2);
        assert_eq!(params.cb_mult, 0);
        assert_eq!(params.cb_luma_mult, 0);
        assert_eq!(params.cb_offset, 0);
        assert_eq!(params.cr_mult, 0);
        assert_eq!(params.cr_luma_mult, 0);
        assert_eq!(params.cr_offset, 0);
        assert!(params.overlap_flag);
        assert!(!params.clip_to_restricted_range);
    }

    #[test]
    fn film_grain_params_monochrome_skips_chroma_fields() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true);
        bits.push_bits(0x2345, 16);
        bits.push_bool(true);
        bits.push_bits(1, 4);
        bits.push_scaling_point(10, 20);
        bits.push_bits(0b11, 2);
        bits.push_bits(0b01, 2);
        for coeff in [0, 1, -1, 2] {
            bits.push_signed_coeff(coeff);
        }
        bits.push_bits(0b00, 2);
        bits.push_bits(0b11, 2);
        bits.push_bool(false);
        bits.push_bool(true);

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) =
            film_grain_params(input, test_ctx(input), true, FrameType::Inter, true, (0, 0))
                .expect("expected parser to parse monochrome film-grain update");

        assert_remaining_position(remaining, &data, consumed_bits);
        let params = expect_update_params(parsed);
        assert_eq!(params.grain_seed, 0x2345);
        assert_eq!(params.scaling_points_y.as_slice(), &[[10, 20]]);
        assert!(!params.chroma_scaling_from_luma);
        assert!(params.scaling_points_cb.is_empty());
        assert!(params.scaling_points_cr.is_empty());
        assert_eq!(params.ar_coeff_lag, 1);
        assert_eq!(params.ar_coeffs_y.as_slice(), &[0, 1, -1, 2]);
        assert_eq!(params.ar_coeffs_cb.as_slice(), &[0]);
        assert_eq!(params.ar_coeffs_cr.as_slice(), &[0]);
        assert_eq!(params.ar_coeff_shift, 6);
        assert_eq!(params.grain_scale_shift, 3);
        assert_eq!(params.cb_mult, 0);
        assert_eq!(params.cb_luma_mult, 0);
        assert_eq!(params.cb_offset, 0);
        assert_eq!(params.cr_mult, 0);
        assert_eq!(params.cr_luma_mult, 0);
        assert_eq!(params.cr_offset, 0);
        assert!(!params.overlap_flag);
        assert!(params.clip_to_restricted_range);
    }

    #[test]
    fn film_grain_params_chroma_scaling_from_luma_reads_chroma_ar_coeffs_without_points() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true);
        bits.push_bits(0x3456, 16);
        bits.push_bool(true);
        bits.push_bits(1, 4);
        bits.push_scaling_point(7, 9);
        bits.push_bool(true);
        bits.push_bits(0b00, 2);
        bits.push_bits(0b01, 2);
        for coeff in [1, 2, 3, 4] {
            bits.push_signed_coeff(coeff);
        }
        for coeff in [5, 6, 7, 8, 9] {
            bits.push_signed_coeff(coeff);
        }
        for coeff in [-1, -2, -3, -4, -5] {
            bits.push_signed_coeff(coeff);
        }
        bits.push_bits(0b11, 2);
        bits.push_bits(0b01, 2);
        bits.push_bool(true);
        bits.push_bool(false);

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) = film_grain_params(
            input,
            test_ctx(input),
            true,
            FrameType::Inter,
            false,
            (0, 0),
        )
        .expect("expected parser to parse chroma_scaling_from_luma branch");

        assert_remaining_position(remaining, &data, consumed_bits);
        let params = expect_update_params(parsed);
        assert_eq!(params.scaling_points_y.as_slice(), &[[7, 9]]);
        assert!(params.chroma_scaling_from_luma);
        assert!(params.scaling_points_cb.is_empty());
        assert!(params.scaling_points_cr.is_empty());
        assert_eq!(params.ar_coeff_lag, 1);
        assert_eq!(params.ar_coeffs_y.as_slice(), &[1, 2, 3, 4]);
        assert_eq!(params.ar_coeffs_cb.as_slice(), &[5, 6, 7, 8, 9]);
        assert_eq!(params.ar_coeffs_cr.as_slice(), &[-1, -2, -3, -4, -5]);
        assert_eq!(params.ar_coeff_shift, 9);
        assert_eq!(params.grain_scale_shift, 1);
        assert_eq!(params.cb_mult, 0);
        assert_eq!(params.cb_luma_mult, 0);
        assert_eq!(params.cb_offset, 0);
        assert_eq!(params.cr_mult, 0);
        assert_eq!(params.cr_luma_mult, 0);
        assert_eq!(params.cr_offset, 0);
        assert!(params.overlap_flag);
        assert!(!params.clip_to_restricted_range);
    }

    #[test]
    fn film_grain_params_reads_chroma_points_and_multiplier_fields_when_present() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true);
        bits.push_bits(0x4567, 16);
        bits.push_bool(true);
        bits.push_bits(1, 4);
        bits.push_scaling_point(1, 2);
        bits.push_bool(false);
        bits.push_bits(1, 4);
        bits.push_scaling_point(30, 40);
        bits.push_bits(1, 4);
        bits.push_scaling_point(50, 60);
        bits.push_bits(0b01, 2);
        bits.push_bits(0b00, 2);
        bits.push_signed_coeff(3);
        bits.push_signed_coeff(-3);
        bits.push_bits(0b10, 2);
        bits.push_bits(0b10, 2);
        bits.push_bits(11, 8);
        bits.push_bits(22, 8);
        bits.push_bits(0x1AB, 9);
        bits.push_bits(33, 8);
        bits.push_bits(44, 8);
        bits.push_bits(0x055, 9);
        bits.push_bool(true);
        bits.push_bool(true);

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) = film_grain_params(
            input,
            test_ctx(input),
            true,
            FrameType::Inter,
            false,
            (0, 0),
        )
        .expect("expected parser to parse explicit chroma points and multipliers");

        assert_remaining_position(remaining, &data, consumed_bits);
        let params = expect_update_params(parsed);
        assert_eq!(params.scaling_points_y.as_slice(), &[[1, 2]]);
        assert!(!params.chroma_scaling_from_luma);
        assert_eq!(params.scaling_points_cb.as_slice(), &[[30, 40]]);
        assert_eq!(params.scaling_points_cr.as_slice(), &[[50, 60]]);
        assert_eq!(params.ar_coeff_lag, 0);
        assert_eq!(params.ar_coeffs_y.as_slice(), &[]);
        assert_eq!(params.ar_coeffs_cb.as_slice(), &[3]);
        assert_eq!(params.ar_coeffs_cr.as_slice(), &[-3]);
        assert_eq!(params.ar_coeff_shift, 8);
        assert_eq!(params.grain_scale_shift, 2);
        assert_eq!(params.cb_mult, 11);
        assert_eq!(params.cb_luma_mult, 22);
        assert_eq!(params.cb_offset, 0x1AB);
        assert_eq!(params.cr_mult, 33);
        assert_eq!(params.cr_luma_mult, 44);
        assert_eq!(params.cr_offset, 0x055);
        assert!(params.overlap_flag);
        assert!(params.clip_to_restricted_range);
    }

    #[test]
    fn film_grain_params_uses_num_pos_luma_for_chroma_when_no_luma_points() {
        let mut bits = BitBuilder::default();
        bits.push_bool(true);
        bits.push_bits(0x5678, 16);
        bits.push_bool(true);
        bits.push_bits(0, 4);
        bits.push_bool(false);
        bits.push_bits(1, 4);
        bits.push_scaling_point(70, 80);
        bits.push_bits(1, 4);
        bits.push_scaling_point(90, 100);
        bits.push_bits(0b00, 2);
        bits.push_bits(0b01, 2);
        for coeff in [10, 11, 12, 13] {
            bits.push_signed_coeff(coeff);
        }
        for coeff in [-10, -11, -12, -13] {
            bits.push_signed_coeff(coeff);
        }
        bits.push_bits(0b01, 2);
        bits.push_bits(0b00, 2);
        bits.push_bits(1, 8);
        bits.push_bits(2, 8);
        bits.push_bits(3, 9);
        bits.push_bits(4, 8);
        bits.push_bits(5, 8);
        bits.push_bits(6, 9);
        bits.push_bool(false);
        bits.push_bool(false);

        let (data, consumed_bits) = with_trailer(bits);
        let input: BitInput = (&data, 0);
        let (remaining, parsed) = film_grain_params(
            input,
            test_ctx(input),
            true,
            FrameType::Inter,
            false,
            (0, 0),
        )
        .expect("expected parser to use num_pos_luma for chroma when no luma points");

        assert_remaining_position(remaining, &data, consumed_bits);
        let params = expect_update_params(parsed);
        assert!(params.scaling_points_y.is_empty());
        assert_eq!(params.scaling_points_cb.as_slice(), &[[70, 80]]);
        assert_eq!(params.scaling_points_cr.as_slice(), &[[90, 100]]);
        assert_eq!(params.ar_coeff_lag, 1);
        assert_eq!(params.ar_coeffs_y.as_slice(), &[]);
        assert_eq!(params.ar_coeffs_cb.as_slice(), &[10, 11, 12, 13]);
        assert_eq!(params.ar_coeffs_cr.as_slice(), &[-10, -11, -12, -13]);
        assert_eq!(params.ar_coeff_shift, 7);
        assert_eq!(params.grain_scale_shift, 0);
        assert_eq!(params.cb_mult, 1);
        assert_eq!(params.cb_luma_mult, 2);
        assert_eq!(params.cb_offset, 3);
        assert_eq!(params.cr_mult, 4);
        assert_eq!(params.cr_luma_mult, 5);
        assert_eq!(params.cr_offset, 6);
        assert!(!params.overlap_flag);
        assert!(!params.clip_to_restricted_range);
    }
}
