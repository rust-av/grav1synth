#[cfg(feature = "diff")]
use std::{borrow::Cow, mem::size_of};

#[cfg(feature = "diff")]
use v_frame::{frame::Frame, pixel::Pixel};

#[cfg(feature = "diff")]
pub fn frame_into_u8<T: Pixel>(frame: &Frame<T>, bit_depth: usize) -> Cow<'_, Frame<u8>> {
    if size_of::<T>() == 1 {
        assert_eq!(bit_depth, 8);
        // SAFETY: We know from the size check that this must be a `Frame<u8>`
        Cow::Borrowed(unsafe { &*(frame as *const Frame<T>).cast::<Frame<u8>>() })
    } else if size_of::<T>() == 2 {
        use std::num::NonZeroU8;

        use v_frame::{chroma::ChromaSubsampling, frame::FrameBuilder};

        assert!(bit_depth > 8 && bit_depth <= 16);
        let mut u8_frame: Frame<u8> = FrameBuilder::new(
            frame.y_plane.width(),
            frame.y_plane.height(),
            frame.subsampling,
            NonZeroU8::new(8).expect("non-zero constant"),
        )
        .build()
        .expect("frame should build");
        for plane in 0..(if frame.subsampling == ChromaSubsampling::Monochrome {
            1
        } else {
            3
        }) {
            let in_plane = match plane {
                0 => &frame.y_plane,
                1 => frame
                    .u_plane
                    .as_ref()
                    .expect("unreachable due to loop bounds"),
                2 => frame
                    .v_plane
                    .as_ref()
                    .expect("unreachable due to loop bounds"),
                _ => unreachable!(),
            };
            let out_plane = match plane {
                0 => &mut u8_frame.y_plane,
                1 => u8_frame
                    .u_plane
                    .as_mut()
                    .expect("unreachable due to loop bounds"),
                2 => u8_frame
                    .v_plane
                    .as_mut()
                    .expect("unreachable due to loop bounds"),
                _ => unreachable!(),
            };

            for (i, o) in in_plane.pixels().zip(out_plane.pixels_mut()) {
                *o = (i.to_u16().expect("i fits in u16") >> (bit_depth - 8usize)) as u8;
            }
        }
        Cow::Owned(u8_frame)
    } else {
        unimplemented!("Bit depths greater than 16 are not currently supported");
    }
}
