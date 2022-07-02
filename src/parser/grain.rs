use std::path::Path;

use anyhow::{anyhow, bail, Result};
use arrayvec::ArrayVec;
use av_format::{
    buffer::AccReader,
    demuxer::{Context as DemuxerContext, Event},
    stream::Stream,
};
use av_ivf::demuxer::IvfDemuxer;
use nom::IResult;

use super::util::BitInput;
use crate::parser::obu::parse_obu;

pub struct FilmGrainParser {
    demuxer: DemuxerContext,
}

impl FilmGrainParser {
    pub fn open<P: AsRef<Path>>(input: P) -> Result<Self> {
        let input = std::fs::File::open(input).unwrap();
        let acc = AccReader::new(input);
        let mut demuxer = DemuxerContext::new(Box::new(IvfDemuxer::new()), Box::new(acc));
        demuxer.read_headers()?;

        Ok(Self { demuxer })
    }

    #[must_use]
    pub fn get_headers(&self) -> &Stream {
        &self.demuxer.info.streams[0]
    }

    pub fn read_packet(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            match self.demuxer.read_event()? {
                Event::NewPacket(packet) => {
                    return Ok(Some(packet.data));
                }
                Event::Continue | Event::MoreDataNeeded(_) => {
                    continue;
                }
                Event::Eof => {
                    return Ok(None);
                }
                Event::NewStream(_) => {
                    bail!("Only one stream per ivf file is supported");
                }
                _ => {
                    unimplemented!("non-exhaustive enum");
                }
            }
        }
    }
}

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

pub fn film_grain_params(input: BitInput) -> IResult<BitInput, FilmGrainHeader> {
    todo!()
}
