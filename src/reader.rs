use std::path::Path;

use anyhow::Result;
use av_decoders::{Decoder, DecoderError, FfmpegDecoder, VideoDetails};
use ffmpeg::{Stream, format::context::Input, media};
use v_frame::{frame::Frame, pixel::Pixel};

pub struct BitstreamReader {
    decoder: Decoder,
}

impl BitstreamReader {
    pub fn open<P: AsRef<Path>>(input: P) -> Result<Self> {
        let decoder = Decoder::from_decoder_impl(av_decoders::DecoderImpl::Ffmpeg(
            FfmpegDecoder::new(input)?,
        ))?;

        Ok(Self { decoder })
    }

    pub fn get_video_stream(&'_ mut self) -> Result<Stream<'_>> {
        Ok(self
            .input()
            .streams()
            .best(media::Type::Video)
            .ok_or(ffmpeg::Error::StreamNotFound)?)
    }

    #[must_use]
    pub fn input(&mut self) -> &mut Input {
        &mut self
            .decoder
            .get_ffmpeg_impl()
            .expect("ffmpeg impl used internally")
            .input_ctx
    }

    #[must_use]
    pub fn get_video_details(&self) -> &VideoDetails {
        self.decoder.get_video_details()
    }

    pub fn get_frame<T: Pixel>(&mut self) -> Result<Option<Frame<T>>> {
        match self.decoder.read_video_frame() {
            Ok(frame) => Ok(Some(frame)),
            Err(DecoderError::EndOfFile) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
