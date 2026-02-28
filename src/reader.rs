use std::{
    num::{NonZeroU8, NonZeroUsize},
    path::Path,
};

use anyhow::{Result, anyhow, bail};
use av1_grain::v_frame::{
    chroma::ChromaSubsampling,
    frame::{Frame as VFrame, FrameBuilder},
    pixel::Pixel as VPixel,
};
use ffmpeg::{
    Rational, Stream,
    codec::{decoder, packet},
    format::{self, context::Input},
    frame, media,
};

pub struct BitstreamReader {
    input_ctx: Input,
    decoder: decoder::Video,
    video_details: VideoDetails,
    frameno: usize,
    end_of_stream: bool,
    eof_sent: bool,
}

impl BitstreamReader {
    pub fn open<P: AsRef<Path>>(input: P) -> Result<Self> {
        ffmpeg::init()?;

        let input_ctx = format::input(&input)?;
        let input = input_ctx
            .streams()
            .best(media::Type::Video)
            .ok_or_else(|| anyhow!("Could not find video stream"))?;
        let mut decoder = ffmpeg::codec::context::Context::from_parameters(input.parameters())?
            .decoder()
            .video()?;
        decoder.set_parameters(input.parameters())?;

        let video_details = VideoDetails {
            width: NonZeroUsize::new(decoder.width() as usize).expect("cannot be zero"),
            height: NonZeroUsize::new(decoder.height() as usize).expect("cannot be zero"),
            // SAFETY: consts
            bit_depth: unsafe {
                match decoder.format() {
                    format::pixel::Pixel::YUV420P
                    | format::pixel::Pixel::YUV422P
                    | format::pixel::Pixel::YUV444P => NonZeroU8::new_unchecked(8),
                    format::pixel::Pixel::YUV420P10LE
                    | format::pixel::Pixel::YUV422P10LE
                    | format::pixel::Pixel::YUV444P10LE => NonZeroU8::new_unchecked(10),
                    format::pixel::Pixel::YUV420P12LE
                    | format::pixel::Pixel::YUV422P12LE
                    | format::pixel::Pixel::YUV444P12LE => NonZeroU8::new_unchecked(12),
                    _ => {
                        bail!("Unsupported pixel format {:?}", decoder.format());
                    }
                }
            },
            chroma_sampling: match decoder.format() {
                format::pixel::Pixel::YUV420P
                | format::pixel::Pixel::YUV420P10LE
                | format::pixel::Pixel::YUV420P12LE => ChromaSubsampling::Yuv420,
                format::pixel::Pixel::YUV422P
                | format::pixel::Pixel::YUV422P10LE
                | format::pixel::Pixel::YUV422P12LE => ChromaSubsampling::Yuv422,
                format::pixel::Pixel::YUV444P
                | format::pixel::Pixel::YUV444P10LE
                | format::pixel::Pixel::YUV444P12LE => ChromaSubsampling::Yuv444,
                _ => {
                    bail!("Unsupported pixel format {:?}", decoder.format());
                }
            },
            frame_rate: input.avg_frame_rate(),
        };

        Ok(Self {
            input_ctx,
            decoder,
            video_details,
            frameno: 0usize,
            end_of_stream: false,
            eof_sent: false,
        })
    }

    pub fn get_video_stream(&'_ self) -> Result<Stream<'_>> {
        Ok(self
            .input_ctx
            .streams()
            .best(media::Type::Video)
            .ok_or(ffmpeg::Error::StreamNotFound)?)
    }

    pub const fn input(&mut self) -> &mut Input {
        &mut self.input_ctx
    }

    #[must_use]
    pub const fn get_video_details(&self) -> &VideoDetails {
        &self.video_details
    }

    pub fn get_frame<T: VPixel>(&mut self) -> Result<Option<VFrame<T>>> {
        // For some reason there's a crap ton of work needed to get ffmpeg to do
        // something simple, because each codec has it's own stupid way of doing
        // things and they don't all decode the same way.
        //
        // Maybe ffmpeg could have made a simple, singular interface that does this for
        // us, but noooooo.
        //
        // Reference: https://ffmpeg.org/doxygen/trunk/api-h264-test_8c_source.html#l00110
        loop {
            // This iterator is actually really stupid... it doesn't reset itself after each
            // `new`. But that solves our lifetime hell issues, ironically.
            let packet = self
                .input_ctx
                .packets()
                .find_map(Result::ok)
                .map(|(_, packet)| packet);

            let mut packet = if let Some(packet) = packet {
                packet
            } else {
                self.end_of_stream = true;
                packet::Packet::empty()
            };

            if self.end_of_stream && !self.eof_sent {
                let _ = self.decoder.send_eof();
                self.eof_sent = true;
            }

            if self.end_of_stream || packet.stream() == self.get_video_stream()?.index() {
                let mut decoded = frame::Video::new(
                    self.decoder.format(),
                    self.video_details.width.get() as u32,
                    self.video_details.height.get() as u32,
                );
                packet.set_pts(Some(self.frameno as i64));
                packet.set_dts(Some(self.frameno as i64));

                if !self.end_of_stream {
                    let _ = self.decoder.send_packet(&packet);
                }

                if self.decoder.receive_frame(&mut decoded).is_ok() {
                    let mut f: VFrame<T> = FrameBuilder::new(
                        self.video_details.width,
                        self.video_details.height,
                        self.video_details.chroma_sampling,
                        self.video_details.bit_depth,
                    )
                    .build()?;

                    f.y_plane.copy_from_u8_slice(decoded.data(0))?;
                    f.u_plane
                        .as_mut()
                        .expect("has chroma")
                        .copy_from_u8_slice(decoded.data(1))?;
                    f.v_plane
                        .as_mut()
                        .expect("has chroma")
                        .copy_from_u8_slice(decoded.data(2))?;

                    self.frameno += 1;
                    return Ok(Some(f));
                } else if self.end_of_stream {
                    return Ok(None);
                }
            }
        }
    }
}

/// Contains important video details
#[derive(Debug, Clone, Copy)]
pub struct VideoDetails {
    /// Width in pixels.
    pub width: NonZeroUsize,
    /// Height in pixels.
    pub height: NonZeroUsize,
    /// Bit-depth of the Video
    pub bit_depth: NonZeroU8,
    /// Chroma Sampling of the Video.
    pub chroma_sampling: ChromaSubsampling,
    /// Frame rate of the Video.
    pub frame_rate: Rational,
}
