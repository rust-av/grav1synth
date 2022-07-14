use std::path::Path;

use anyhow::Result;
use ffmpeg::{
    format::{self, context::Input},
    media,
    Stream,
};

pub struct BitstreamReader {
    input_ctx: Input,
}

impl BitstreamReader {
    pub fn open<P: AsRef<Path>>(input: P) -> Result<Self> {
        ffmpeg::init()?;

        let input_ctx = format::input(&input)?;

        Ok(Self { input_ctx })
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
}
