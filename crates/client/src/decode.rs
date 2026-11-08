//! What the host sends, taken apart: the agent-upload request that may come
//! before the agent starts, then FeroStream frames.
//!
//! [`Decoder`] is the one place that reads the host's bytes. The standalone
//! client runs it in its reader thread. A program that embeds the client
//! can run it in a separate, sandboxed process instead (FeroSSH does) with
//! `unpack` on: compressed tiles then leave the decoder as raw pixels, so
//! the process with the windows only ever sees frames that are plain,
//! size-checked data ([`crate::win32::Settings::raw_only`]).

use ferostream_proto::tile::{Encoding, Tile};
use ferostream_proto::{FrameReader, Message};

use crate::link::UploadScan;

/// One thing found in the host's bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    /// The host asks for the agent for this machine kind (`uname -m`),
    /// before the agent runs.
    Upload(String),
    Message(Message),
}

/// Splits the host's byte stream; see the module text.
#[derive(Debug)]
pub struct Decoder {
    scan: UploadScan,
    reader: FrameReader,
    unpack: bool,
}

impl Decoder {
    /// A decoder for the bytes from the start of the connection. With
    /// `unpack`, compressed tiles come out raw.
    pub fn new(unpack: bool) -> Decoder {
        Decoder {
            scan: UploadScan::default(),
            reader: FrameReader::after_preamble(),
            unpack,
        }
    }

    /// Takes the next bytes; what they complete goes to `out`. An error
    /// ends the connection: the stream cannot be trusted after it.
    pub fn push(&mut self, data: &[u8], out: &mut Vec<Output>) -> Result<(), String> {
        if let Some(arch) = self.scan.push(data) {
            out.push(Output::Upload(arch));
        }
        self.reader.push(data);
        loop {
            match self.reader.next_message() {
                Ok(Some(Message::Tile(t))) if self.unpack => {
                    out.push(Output::Message(Message::Tile(unpack(t)?)));
                }
                Ok(Some(m)) => out.push(Output::Message(m)),
                Ok(None) => return Ok(()),
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

/// The tile with its pixels raw.
pub fn unpack(t: Tile) -> Result<Tile, String> {
    if t.encoding == Encoding::Raw {
        t.check().map_err(|e| e.to_string())?;
        return Ok(t);
    }
    let data = t.decode().map_err(|e| e.to_string())?;
    Ok(Tile {
        encoding: Encoding::Raw,
        data,
        ..t
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use super::*;
    use ferostream_proto::PREAMBLE;

    #[test]
    fn upload_then_frames_unpacked() {
        let px: Vec<u8> = (0..16 * 8 * 4).map(|i| (i % 5) as u8).collect();
        let tile = Tile::lz4(2, 1, 1, 16, 8, &px);
        let mut bytes = b"motd\n\0FEROSTREAM-UPLOAD x86_64\n".to_vec();
        bytes.extend_from_slice(PREAMBLE);
        bytes.extend(Message::Ping { token: 1 }.encode());
        bytes.extend(Message::Tile(tile.clone()).encode());

        let mut d = Decoder::new(true);
        let mut out = Vec::new();
        // Byte by byte: nothing depends on how the stream is cut.
        for b in &bytes {
            d.push(std::slice::from_ref(b), &mut out).unwrap();
        }
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], Output::Upload("x86_64".into()));
        assert_eq!(out[1], Output::Message(Message::Ping { token: 1 }));
        match &out[2] {
            Output::Message(Message::Tile(t)) => {
                assert_eq!(t.encoding, Encoding::Raw);
                assert_eq!(t.data, px);
            }
            other => panic!("{other:?}"),
        }

        let mut d = Decoder::new(false);
        let mut out = Vec::new();
        d.push(&bytes, &mut out).unwrap();
        assert_eq!(out[2], Output::Message(Message::Tile(tile)));
    }

    #[test]
    fn bad_tile_ends_the_stream() {
        let mut t = Tile::lz4(2, 0, 0, 16, 16, &[0; 1024]);
        t.data.truncate(6);
        let mut bytes = PREAMBLE.to_vec();
        bytes.extend(Message::Tile(t).encode());
        let mut d = Decoder::new(true);
        assert!(d.push(&bytes, &mut Vec::new()).is_err());
    }
}
