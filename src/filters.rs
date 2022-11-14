use anyhow::{anyhow, bail, Result};
use av1_grain::v_frame::{frame::Frame, prelude::Pixel};
use video_resize::algorithms::{
    BicubicCatmullRom, BicubicHermite, BicubicMitchell, Lanczos3, Spline36,
};
use video_resize::{crop, resize, CropDimensions, ResizeDimensions};

pub struct FilterChain {
    filters: Vec<Filter>,
}

impl FilterChain {
    pub fn new(filters: &str) -> Result<Self> {
        if filters.is_empty() {
            return Ok(Self {
                filters: Vec::new(),
            });
        }

        let mut parsed = Vec::new();
        for filter in filters.split(';') {
            let (filter, args) = filter
                .split_once(':')
                .ok_or_else(|| anyhow!("Invalid filter syntax in \"{}\"", filter))?;
            let args = args.split(',');
            match filter {
                "crop" => {
                    let (mut top, mut bottom, mut left, mut right) = (0, 0, 0, 0);
                    for arg in args {
                        let (arg, value) = arg
                            .split_once('=')
                            .ok_or_else(|| anyhow!("Invalid filter syntax in \"{}\"", arg))?;
                        match arg {
                            "top" => {
                                top = value.parse()?;
                            }
                            "bottom" => {
                                bottom = value.parse()?;
                            }
                            "left" => {
                                left = value.parse()?;
                            }
                            "right" => {
                                right = value.parse()?;
                            }
                            arg => bail!("Unrecognized crop arg \"{}\"", arg),
                        }
                    }
                    parsed.push(Filter::Crop {
                        top,
                        bottom,
                        left,
                        right,
                    });
                }
                "resize" => {
                    let (mut width, mut height, mut alg) = (0, 0, "catmullrom");
                    for arg in args {
                        let (arg, value) = arg
                            .split_once('=')
                            .ok_or_else(|| anyhow!("Invalid filter syntax in \"{}\"", arg))?;
                        match arg {
                            "width" => {
                                width = value.parse()?;
                            }
                            "height" => {
                                height = value.parse()?;
                            }
                            "alg" => match value {
                                "hermite" => {
                                    alg = "hermite";
                                }
                                "catmullrom" => {
                                    alg = "catmullrom";
                                }
                                "mitchell" => {
                                    alg = "mitchell";
                                }
                                "lanczos" => {
                                    alg = "lanczos";
                                }
                                "spline36" => {
                                    alg = "spline36";
                                }
                                alg => bail!("Unrecognized resize algorithm \"{}\"", alg),
                            },
                            arg => bail!("Unrecognized resize arg \"{}\"", arg),
                        }
                    }
                    if width == 0 || height == 0 {
                        bail!("Both width and height must be provided to resize filter");
                    }
                    parsed.push(Filter::Resize { width, height, alg });
                }
                f => bail!("Unrecognized filter \"{}\"", f),
            }
        }

        Ok(Self { filters: parsed })
    }

    pub fn apply<T: Pixel>(&self, frame: Frame<T>, source_bd: usize) -> Frame<T> {
        self.filters
            .iter()
            .fold(frame, |prev, f| f.apply(&prev, source_bd))
    }
}

enum Filter {
    Crop {
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
    },
    Resize {
        width: usize,
        height: usize,
        alg: &'static str,
    },
}

impl Filter {
    pub fn apply<T: Pixel>(&self, frame: &Frame<T>, source_bd: usize) -> Frame<T> {
        match *self {
            Filter::Crop {
                top,
                bottom,
                left,
                right,
            } => crop(
                frame,
                CropDimensions {
                    top,
                    bottom,
                    left,
                    right,
                },
            )
            .unwrap(),
            Filter::Resize { width, height, alg } => match alg {
                "hermite" => resize::<T, BicubicHermite>(
                    frame,
                    ResizeDimensions { width, height },
                    source_bd,
                )
                .unwrap(),
                "catmullrom" => resize::<T, BicubicCatmullRom>(
                    frame,
                    ResizeDimensions { width, height },
                    source_bd,
                )
                .unwrap(),
                "mitchell" => resize::<T, BicubicMitchell>(
                    frame,
                    ResizeDimensions { width, height },
                    source_bd,
                )
                .unwrap(),
                "lanczos" => {
                    resize::<T, Lanczos3>(frame, ResizeDimensions { width, height }, source_bd)
                        .unwrap()
                }
                "spline36" => {
                    resize::<T, Spline36>(frame, ResizeDimensions { width, height }, source_bd)
                        .unwrap()
                }
                _ => unreachable!(),
            },
        }
    }
}
