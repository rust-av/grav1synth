use std::path::Path;

use anyhow::Result;
use ffmpeg::{
    format::{self, context::Input},
    media,
    packet,
    Packet,
    Stream,
};

pub struct BitstreamReader {
    input_ctx: Input,
    end_of_stream: bool,
    stream_index: usize,
}

impl BitstreamReader {
    pub fn open<P: AsRef<Path>>(input: P) -> Result<Self> {
        ffmpeg::init()?;

        let input_ctx = format::input(&input)?;

        let mut this = Self {
            input_ctx,
            end_of_stream: false,
            stream_index: 0,
        };
        this.stream_index = this.get_video_stream()?.index();

        Ok(this)
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

    pub fn read_packet(&mut self) -> Option<Packet> {
        // For some reason there's a crap ton of work needed to get ffmpeg to do
        // something simple, because each codec has it's own stupid way of doing
        // things and they don't all decode the same way.
        //
        // Maybe ffmpeg could have made a simple, singular interface that does this for
        // us, but noooooo.
        //
        // Reference: https://ffmpeg.org/doxygen/trunk/api-h264-test_8c_source.html#l00110
        if self.end_of_stream {
            return None;
        }

        loop {
            // This iterator is actually really stupid... it doesn't reset itself after each
            // `new`. But that solves our lifetime hell issues, ironically.
            let packet = self.input_ctx.packets().next().map(|(_, packet)| packet);

            let packet = if let Some(packet) = packet {
                packet
            } else {
                self.end_of_stream = true;
                packet::Packet::empty()
            };

            if packet.stream() == self.stream_index {
                return Some(packet);
            }

            if self.end_of_stream {
                return None;
            }
        }
    }
}
