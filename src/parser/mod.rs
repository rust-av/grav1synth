mod obu;
mod spec;
mod util;

use std::path::Path;

use anyhow::{bail, Result};
use av_format::{
    buffer::AccReader,
    demuxer::{Context as DemuxerContext, Event},
};
use av_ivf::demuxer::IvfDemuxer;

pub struct BitstreamParser {
    demuxer: DemuxerContext,
}

impl BitstreamParser {
    pub fn open<P: AsRef<Path>>(input: P) -> Result<Self> {
        let input = std::fs::File::open(input).unwrap();
        let acc = AccReader::new(input);
        let mut demuxer = DemuxerContext::new(Box::new(IvfDemuxer::new()), Box::new(acc));
        demuxer.read_headers()?;

        Ok(Self { demuxer })
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

pub struct PacketData {
    //
}

pub fn decode_packet(raw_packet: &[u8]) -> Result<PacketData> {
    //
}
