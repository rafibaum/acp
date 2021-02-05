#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Packet {
    #[prost(oneof = "packet::Data", tags = "1, 2, 3")]
    pub data: ::core::option::Option<packet::Data>,
}
/// Nested message and enum types in `Packet`.
pub mod packet {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Data {
        #[prost(message, tag = "1")]
        Stream(super::StreamPacket),
        #[prost(message, tag = "2")]
        Handshake(super::Handshake),
        #[prost(message, tag = "3")]
        Terminate(super::Terminate),
    }
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Handshake {
    #[prost(string, tag = "1")]
    pub name: ::prost::alloc::string::String,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Terminate {}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StreamPacket {
    #[prost(uint32, tag = "1")]
    pub seq: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub data: ::prost::alloc::vec::Vec<u8>,
}
