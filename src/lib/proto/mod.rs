//! A container for automatically generated Protobuf containers from the `prost-build` crate.

#[allow(missing_docs)]
mod packets;

use crate::AcpError;
use bytes::{Buf, BytesMut};
use packet::Data;
pub use packets::*;
use prost::Message;
use std::net::IpAddr;

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

pub fn frame(buf: &mut BytesMut) -> Result<Option<Packet>, AcpError> {
    let mut tmp_buf = &buf[..];
    let len = match prost::decode_length_delimiter(tmp_buf) {
        Ok(len) => len,
        Err(e) => {
            if tmp_buf.len() > 10 {
                return Err(AcpError::MalformedLengthDelimiter(e));
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

    let packet = Packet::decode(&tmp_buf[..len]).map_err(|e| AcpError::MalformedPacket(e))?;
    buf.advance(len_delim_len + len);
    Ok(Some(packet))
}
