use std::{num::NonZeroUsize, path::Path};

use anyhow::{Result, bail};
use av1_grain::v_frame::{
    chroma::ChromaSubsampling,
    frame::{Frame, FrameBuilder},
    pixel::Pixel,
};
use ffmpeg::{
    Stream,
    codec::{decoder, packet},
    format::{self, context::Input},
    frame, media,
};
use num_rational::Rational32;

pub struct BitstreamReader {
    input_ctx: Input,
    decoder: decoder::Video,
    video_details: VideoDetails,
    stream_index: usize,
    frameno: usize,
    end_of_stream: bool,
    eof_sent: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct VideoDetails {
    pub width: usize,
    pub height: usize,
    pub bit_depth: usize,
    pub chroma_sampling: ChromaSubsampling,
    pub frame_rate: Rational32,
}

impl BitstreamReader {
    pub fn open<P: AsRef<Path>>(input: P) -> Result<Self> {
        ffmpeg::init()?;

        let input_ctx = format::input(input.as_ref())?;
        let stream = input_ctx
            .streams()
            .best(media::Type::Video)
            .ok_or(ffmpeg::Error::StreamNotFound)?;
        let stream_index = stream.index();

        let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
        let mut decoder = context.decoder().video()?;
        decoder.set_parameters(stream.parameters())?;

        let bit_depth = match decoder.format() {
            format::pixel::Pixel::YUV420P
            | format::pixel::Pixel::YUV422P
            | format::pixel::Pixel::YUV444P
            | format::pixel::Pixel::YUVJ420P
            | format::pixel::Pixel::YUVJ422P
            | format::pixel::Pixel::YUVJ444P => 8,
            format::pixel::Pixel::YUV420P10LE
            | format::pixel::Pixel::YUV422P10LE
            | format::pixel::Pixel::YUV444P10LE => 10,
            format::pixel::Pixel::YUV420P12LE
            | format::pixel::Pixel::YUV422P12LE
            | format::pixel::Pixel::YUV444P12LE => 12,
            fmt => {
                bail!("unsupported video format {fmt:?}");
            }
        };

        let chroma_sampling = match decoder.format() {
            format::pixel::Pixel::YUV420P
            | format::pixel::Pixel::YUVJ420P
            | format::pixel::Pixel::YUV420P10LE
            | format::pixel::Pixel::YUV420P12LE => ChromaSubsampling::Yuv420,
            format::pixel::Pixel::YUV422P
            | format::pixel::Pixel::YUVJ422P
            | format::pixel::Pixel::YUV422P10LE
            | format::pixel::Pixel::YUV422P12LE => ChromaSubsampling::Yuv422,
            format::pixel::Pixel::YUV444P
            | format::pixel::Pixel::YUVJ444P
            | format::pixel::Pixel::YUV444P10LE
            | format::pixel::Pixel::YUV444P12LE => ChromaSubsampling::Yuv444,
            fmt => {
                bail!("unsupported video format {fmt:?}");
            }
        };

        let mut frame_rate = stream.avg_frame_rate();
        if frame_rate.denominator() == 0 {
            frame_rate = stream.rate();
        }

        Ok(Self {
            video_details: VideoDetails {
                width: decoder.width() as usize,
                height: decoder.height() as usize,
                bit_depth,
                chroma_sampling,
                frame_rate: Rational32::new(frame_rate.numerator(), frame_rate.denominator()),
            },
            input_ctx,
            decoder,
            stream_index,
            frameno: 0,
            end_of_stream: false,
            eof_sent: false,
        })
    }

    pub fn get_video_stream(&self) -> Result<Stream<'_>> {
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

    pub fn get_frame<T: Pixel>(&mut self) -> Result<Option<Frame<T>>> {
        loop {
            let packet = self
                .input_ctx
                .packets()
                .next()
                .and_then(Result::ok)
                .map(|(_, pkt)| pkt);

            let mut packet = if let Some(pkt) = packet {
                pkt
            } else {
                self.end_of_stream = true;
                packet::Packet::empty()
            };

            if self.end_of_stream && !self.eof_sent {
                let _ = self.decoder.send_eof();
                self.eof_sent = true;
            }

            if self.end_of_stream || packet.stream() == self.stream_index {
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
                    let frame = decode_frame::<T>(&self.video_details, &decoded)?;
                    self.frameno += 1;
                    return Ok(Some(frame));
                } else if self.end_of_stream {
                    return Ok(None);
                }
            }
        }
    }
}

fn decode_frame<T: Pixel>(details: &VideoDetails, decoded: &frame::Video) -> Result<Frame<T>> {
    let width = details.width;
    let height = details.height;

    let nz_width = NonZeroUsize::new(width)
        .ok_or_else(|| anyhow::anyhow!("zero-width resolution is not supported"))?;
    let nz_height = NonZeroUsize::new(height)
        .ok_or_else(|| anyhow::anyhow!("zero-height resolution is not supported"))?;
    let nz_bd = std::num::NonZeroU8::new(details.bit_depth as u8)
        .ok_or_else(|| anyhow::anyhow!("zero bit-depth is not supported"))?;

    let mut frame: Frame<T> =
        FrameBuilder::new(nz_width, nz_height, details.chroma_sampling, nz_bd)
            .build()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

    let y_stride = NonZeroUsize::new(decoded.stride(0))
        .ok_or_else(|| anyhow::anyhow!("luma stride is zero"))?;
    frame
        .y_plane
        .copy_from_u8_slice_with_stride(decoded.data(0), y_stride)
        .map_err(|e| anyhow::anyhow!("luma plane copy failed: {e}"))?;

    if let Some(u_plane) = frame.u_plane.as_mut() {
        let u_stride = NonZeroUsize::new(decoded.stride(1))
            .ok_or_else(|| anyhow::anyhow!("U chroma stride is zero"))?;
        u_plane
            .copy_from_u8_slice_with_stride(decoded.data(1), u_stride)
            .map_err(|e| anyhow::anyhow!("U chroma plane copy failed: {e}"))?;
    }

    if let Some(v_plane) = frame.v_plane.as_mut() {
        let v_stride = NonZeroUsize::new(decoded.stride(2))
            .ok_or_else(|| anyhow::anyhow!("V chroma stride is zero"))?;
        v_plane
            .copy_from_u8_slice_with_stride(decoded.data(2), v_stride)
            .map_err(|e| anyhow::anyhow!("V chroma plane copy failed: {e}"))?;
    }

    Ok(frame)
}
