use std::num::{NonZeroU8, NonZeroUsize};

use anyhow::{Result, anyhow, bail};
use av1_grain::v_frame::frame::Frame;
use av1_grain::v_frame::pixel::Pixel;
use video_resize::algorithms::{
    BicubicCatmullRom, BicubicHermite, BicubicMitchell, Lanczos3, Spline36,
};
use video_resize::{CropDimensions, ResizeDimensions, crop, resize};

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
                .ok_or_else(|| anyhow!("Invalid filter syntax in \"{filter}\""))?;
            let args = args.split(',');
            match filter {
                "crop" => {
                    let (mut top, mut bottom, mut left, mut right) = (0, 0, 0, 0);
                    for arg in args {
                        let (arg, value) = arg
                            .split_once('=')
                            .ok_or_else(|| anyhow!("Invalid filter syntax in \"{arg}\""))?;
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
                            arg => bail!("Unrecognized crop arg \"{arg}\""),
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
                            .ok_or_else(|| anyhow!("Invalid filter syntax in \"{arg}\""))?;
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
                                alg => bail!("Unrecognized resize algorithm \"{alg}\""),
                            },
                            arg => bail!("Unrecognized resize arg \"{arg}\""),
                        }
                    }
                    if width == 0 || height == 0 {
                        bail!("Both width and height must be provided to resize filter");
                    }
                    // SAFETY: checked above
                    unsafe {
                        parsed.push(Filter::Resize {
                            width: NonZeroUsize::new_unchecked(width),
                            height: NonZeroUsize::new_unchecked(height),
                            alg,
                        });
                    }
                }
                f => bail!("Unrecognized filter \"{f}\""),
            }
        }

        Ok(Self { filters: parsed })
    }

    pub fn apply<T: Pixel>(&self, frame: Frame<T>, source_bd: NonZeroU8) -> Frame<T> {
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
        width: NonZeroUsize,
        height: NonZeroUsize,
        alg: &'static str,
    },
}

impl Filter {
    pub fn apply<T: Pixel>(&self, frame: &Frame<T>, source_bd: NonZeroU8) -> Frame<T> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_new_error_contains(filters: &str, expected: &str) {
        let Err(err) = FilterChain::new(filters) else {
            panic!("expected parsing error for \"{filters}\"")
        };
        let message = err.to_string();
        assert!(
            message.contains(expected),
            "expected error to contain \"{expected}\", got \"{message}\""
        );
    }

    #[test]
    fn new_accepts_empty_filter_chain() {
        let chain = FilterChain::new("").unwrap();
        assert!(chain.filters.is_empty());
    }

    #[test]
    fn new_parses_crop_filter_args() {
        let chain = FilterChain::new("crop:top=1,bottom=2,left=3,right=4").unwrap();
        assert_eq!(chain.filters.len(), 1);

        match &chain.filters[0] {
            Filter::Crop {
                top,
                bottom,
                left,
                right,
            } => {
                assert_eq!(*top, 1);
                assert_eq!(*bottom, 2);
                assert_eq!(*left, 3);
                assert_eq!(*right, 4);
            }
            Filter::Resize { .. } => panic!("expected crop filter"),
        }
    }

    #[test]
    fn new_parses_resize_filter_with_default_algorithm() {
        let chain = FilterChain::new("resize:width=1920,height=1080").unwrap();
        assert_eq!(chain.filters.len(), 1);

        match &chain.filters[0] {
            Filter::Resize { width, height, alg } => {
                assert_eq!(width.get(), 1920);
                assert_eq!(height.get(), 1080);
                assert_eq!(*alg, "catmullrom");
            }
            Filter::Crop { .. } => panic!("expected resize filter"),
        }
    }

    #[test]
    fn new_parses_resize_filter_with_all_supported_algorithms() {
        for alg in ["hermite", "catmullrom", "mitchell", "lanczos", "spline36"] {
            let filter = format!("resize:width=640,height=360,alg={alg}");
            let chain = FilterChain::new(&filter).unwrap();

            match &chain.filters[0] {
                Filter::Resize {
                    width,
                    height,
                    alg: parsed_alg,
                } => {
                    assert_eq!(width.get(), 640);
                    assert_eq!(height.get(), 360);
                    assert_eq!(*parsed_alg, alg);
                }
                Filter::Crop { .. } => panic!("expected resize filter"),
            }
        }
    }

    #[test]
    fn new_parses_multiple_filters_in_order() {
        let chain = FilterChain::new("crop:top=4;resize:width=320,height=240,alg=lanczos").unwrap();
        assert_eq!(chain.filters.len(), 2);

        match &chain.filters[0] {
            Filter::Crop {
                top,
                bottom,
                left,
                right,
            } => {
                assert_eq!(*top, 4);
                assert_eq!(*bottom, 0);
                assert_eq!(*left, 0);
                assert_eq!(*right, 0);
            }
            Filter::Resize { .. } => panic!("expected crop filter"),
        }

        match &chain.filters[1] {
            Filter::Resize { width, height, alg } => {
                assert_eq!(width.get(), 320);
                assert_eq!(height.get(), 240);
                assert_eq!(*alg, "lanczos");
            }
            Filter::Crop { .. } => panic!("expected resize filter"),
        }
    }

    #[test]
    fn new_rejects_filter_without_colon_separator() {
        assert_new_error_contains("crop", "Invalid filter syntax in \"crop\"");
    }

    #[test]
    fn new_rejects_unrecognized_filter() {
        assert_new_error_contains("rotate:degrees=90", "Unrecognized filter \"rotate\"");
    }

    #[test]
    fn new_rejects_crop_arg_without_equals_separator() {
        assert_new_error_contains("crop:top", "Invalid filter syntax in \"top\"");
    }

    #[test]
    fn new_rejects_unrecognized_crop_arg() {
        assert_new_error_contains("crop:width=12", "Unrecognized crop arg \"width\"");
    }

    #[test]
    fn new_rejects_non_numeric_crop_value() {
        assert_new_error_contains("crop:top=abc", "invalid digit found in string");
    }

    #[test]
    fn new_rejects_resize_arg_without_equals_separator() {
        assert_new_error_contains(
            "resize:width=640,height",
            "Invalid filter syntax in \"height\"",
        );
    }

    #[test]
    fn new_rejects_unrecognized_resize_arg() {
        assert_new_error_contains(
            "resize:width=640,height=360,scale=2",
            "Unrecognized resize arg \"scale\"",
        );
    }

    #[test]
    fn new_rejects_unrecognized_resize_algorithm() {
        assert_new_error_contains(
            "resize:width=640,height=360,alg=nearest",
            "Unrecognized resize algorithm \"nearest\"",
        );
    }

    #[test]
    fn new_rejects_resize_when_width_or_height_missing() {
        assert_new_error_contains(
            "resize:width=640",
            "Both width and height must be provided to resize filter",
        );
        assert_new_error_contains(
            "resize:height=360",
            "Both width and height must be provided to resize filter",
        );
    }

    #[test]
    fn new_rejects_non_numeric_resize_dimensions() {
        assert_new_error_contains(
            "resize:width=wide,height=360",
            "invalid digit found in string",
        );
        assert_new_error_contains(
            "resize:width=640,height=tall",
            "invalid digit found in string",
        );
    }
}
