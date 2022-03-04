use std::path::Path;

use av_format::{buffer::AccReader, demuxer::Context as DemuxerContext};
use av_ivf::demuxer::IvfDemuxer;

pub struct BitstreamParser {
    demuxer: DemuxerContext,
}

impl BitstreamParser {
    pub fn open<P: AsRef<Path>>(input: P) -> Self {
        let input = std::fs::File::open(input).unwrap();
        let acc = AccReader::new(input);
        let demuxer = DemuxerContext::new(Box::new(IvfDemuxer::new()), Box::new(acc));
        Self { demuxer }
    }
}
