use std::path::Path;

use anyhow::{anyhow, bail, Result};
use av1_grain::v_frame::{frame::Frame as VFrame, pixel::Pixel as VPixel, prelude::ChromaSampling};
use ffmpeg::{
    codec::{decoder, packet},
    format::{self, context::Input},
    frame, media, Rational, Stream,
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
            width: decoder.width() as usize,
            height: decoder.height() as usize,
            bit_depth: match decoder.format() {
                format::pixel::Pixel::YUV420P
                | format::pixel::Pixel::YUV422P
                | format::pixel::Pixel::YUV444P => 8,
                format::pixel::Pixel::YUV420P10LE
                | format::pixel::Pixel::YUV422P10LE
                | format::pixel::Pixel::YUV444P10LE => 10,
                format::pixel::Pixel::YUV420P12LE
                | format::pixel::Pixel::YUV422P12LE
                | format::pixel::Pixel::YUV444P12LE => 12,
                _ => {
                    bail!("Unsupported pixel format {:?}", decoder.format());
                }
            },
            chroma_sampling: match decoder.format() {
                format::pixel::Pixel::YUV420P
                | format::pixel::Pixel::YUV420P10LE
                | format::pixel::Pixel::YUV420P12LE => ChromaSampling::Cs420,
                format::pixel::Pixel::YUV422P
                | format::pixel::Pixel::YUV422P10LE
                | format::pixel::Pixel::YUV422P12LE => ChromaSampling::Cs422,
                format::pixel::Pixel::YUV444P
                | format::pixel::Pixel::YUV444P10LE
                | format::pixel::Pixel::YUV444P12LE => ChromaSampling::Cs444,
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

    pub fn get_video_stream(&self) -> Result<Stream> {
        Ok(self
            .input_ctx
            .streams()
            .best(media::Type::Video)
            .ok_or(ffmpeg::Error::StreamNotFound)?)
    }

    pub fn input(&mut self) -> &mut Input {
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
                    self.video_details.width as u32,
                    self.video_details.height as u32,
                );
                packet.set_pts(Some(self.frameno as i64));
                packet.set_dts(Some(self.frameno as i64));

                if !self.end_of_stream {
                    let _ = self.decoder.send_packet(&packet);
                }

                if self.decoder.receive_frame(&mut decoded).is_ok() {
                    let mut f: VFrame<T> = VFrame::new_with_padding(
                        self.video_details.width,
                        self.video_details.height,
                        self.video_details.chroma_sampling,
                        0,
                    );
                    let width = self.video_details.width;
                    let height = self.video_details.height;
                    let bit_depth = self.video_details.bit_depth;
                    let bytes = if bit_depth > 8 { 2 } else { 1 };
                    let (chroma_width, chroma_height) = self
                        .video_details
                        .chroma_sampling
                        .get_chroma_dimensions(width, height);

                    // `VFrame::new_with_padding` expands the width to a factor of 8.
                    // We don't want this.
                    // To be honest, this is probably a bug in v_frame but I'm scared to change it
                    // since so many packages use v_frame.
                    f.planes[0].cfg.width = width;
                    f.planes[0].cfg.height = height;
                    f.planes[1].cfg.width = chroma_width;
                    f.planes[1].cfg.height = chroma_height;
                    f.planes[2].cfg.width = chroma_width;
                    f.planes[2].cfg.height = chroma_height;

                    f.planes[0].copy_from_raw_u8(decoded.data(0), width * bytes, bytes);
                    f.planes[1].copy_from_raw_u8(decoded.data(1), chroma_width * bytes, bytes);
                    f.planes[2].copy_from_raw_u8(decoded.data(2), chroma_width * bytes, bytes);

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
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Bit-depth of the Video
    pub bit_depth: usize,
    /// Chroma Sampling of the Video.
    pub chroma_sampling: ChromaSampling,
    /// Frame rate of the Video.
    pub frame_rate: Rational,
}
