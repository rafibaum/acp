//! A container for automatically generated Protobuf containers from the `prost-build` crate.

#[allow(missing_docs)]
mod packets;

use bytes::{Buf, BytesMut};
use packet::Data;
pub use packets::*;
use prost::Message;
use std::error::Error;
use std::fmt::{Display, Formatter};
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

/// Attempts to take a stream of bytes and frame it as a packet. If successful, the buffer is
/// advanced past the bytes used for the packet.
pub fn frame(buf: &mut BytesMut) -> Result<Option<Packet>, AcpFrameError> {
    let mut tmp_buf = &buf[..];
    let len = match prost::decode_length_delimiter(tmp_buf) {
        Ok(len) => len,
        Err(e) => {
            if tmp_buf.len() > 10 {
                return Err(AcpFrameError::MalformedLengthDelimiter(e));
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

    let packet = Packet::decode(&tmp_buf[..len]).map_err(|e| AcpFrameError::MalformedPacket(e))?;
    buf.advance(len_delim_len + len);
    Ok(Some(packet))
}

/// Errors which can be returned when framing a packet.
#[derive(Debug)]
pub enum AcpFrameError {
    /// The length delimiter of a packet could not be decoded
    MalformedLengthDelimiter(prost::DecodeError),
    /// The packet could not be decoded (although a length delimiter was decoded, but may not be
    /// correct).
    MalformedPacket(prost::DecodeError),
}

impl Error for AcpFrameError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            AcpFrameError::MalformedLengthDelimiter(err) => Some(err),
            AcpFrameError::MalformedPacket(err) => Some(err),
        }
    }
}

impl Display for AcpFrameError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpFrameError::MalformedLengthDelimiter(_) => write!(
                f,
                "encountered a malformed length delimiter while framing a packet"
            ),
            AcpFrameError::MalformedPacket(_) => {
                write!(f, "encountered a malformed packet during framing")
            }
        }
    }
}
