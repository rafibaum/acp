//! A container for automatically generated Protobuf containers from the `prost-build` crate.

#[allow(missing_docs)]
mod packets;

use crate::proto::PacketFrameError::{MalformedLengthDelimiter, MalformedPacket};
use bytes::{Buf, BytesMut};
use packet::Data;
pub use packets::*;
use prost::Message;
use std::net::IpAddr;
use thiserror::Error;

impl Packet {
    /// Create a new packet of the specified type.
    /// Convenience constructor for the auto-generated packet types since they can get unwieldy
    /// otherwise.
    pub fn new(data: Data) -> Self {
        Packet { data: Some(data) }
    }
}

impl From<IpAddr> for IpAddress {
    fn from(addr: IpAddr) -> IpAddress {
        match addr {
            IpAddr::V4(addr) => {
                let octets = Vec::from(addr.octets());
                IpAddress {
                    address: Some(ip_address::Address::V4(octets)),
                }
            }
            IpAddr::V6(addr) => {
                let octets = Vec::from(addr.octets());
                IpAddress {
                    address: Some(ip_address::Address::V6(octets)),
                }
            }
        }
    }
}

pub fn frame(buf: &mut BytesMut) -> Result<Option<Packet>, PacketFrameError> {
    let mut tmp_buf = &buf[..];
    let len = match prost::decode_length_delimiter(tmp_buf) {
        Ok(len) => len,
        Err(e) => {
            if tmp_buf.len() > 10 {
                return Err(MalformedLengthDelimiter(e));
            } else {
                return Ok(None);
            }
        }
    };

    let len_delim_len = prost::length_delimiter_len(len);
    tmp_buf.advance(len_delim_len);
    if tmp_buf.remaining() < len {
        return Ok(None);
    }

    let packet = Packet::decode(&tmp_buf[..len]).map_err(|e| MalformedPacket(e))?;
    buf.advance(len_delim_len + len);
    Ok(Some(packet))
}

#[derive(Error, Debug)]
pub enum PacketFrameError {
    #[error("encountered a malformed length delimiter while framing a packet")]
    MalformedLengthDelimiter(prost::DecodeError),
    #[error("encountered a malformed packet during framing")]
    MalformedPacket(prost::DecodeError),
}
