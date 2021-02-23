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
        Ping(super::Ping),
        #[prost(message, tag = "2")]
        StartBenchmark(super::StartBenchmark),
        #[prost(message, tag = "3")]
        StopBenchmark(super::StopBenchmark),
    }
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Datagram {
    #[prost(oneof = "datagram::Data", tags = "1")]
    pub data: ::core::option::Option<datagram::Data>,
}
/// Nested message and enum types in `Datagram`.
pub mod datagram {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Data {
        #[prost(message, tag = "1")]
        BenchmarkPayload(super::BenchmarkPayload),
    }
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Ping {
    #[prost(string, tag = "1")]
    pub data: ::prost::alloc::string::String,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StartBenchmark {}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StopBenchmark {}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct BenchmarkPayload {
    #[prost(bytes = "vec", tag = "1")]
    pub payload: ::prost::alloc::vec::Vec<u8>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ValidationToken {
    #[prost(bytes = "vec", tag = "1")]
    pub payload: ::prost::alloc::vec::Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub signature: ::prost::alloc::vec::Vec<u8>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ValidationPayload {
    #[prost(bytes = "vec", tag = "1")]
    pub salt: ::prost::alloc::vec::Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub secs: u64,
    #[prost(message, optional, tag = "3")]
    pub address: ::core::option::Option<IpAddress>,
    #[prost(uint32, tag = "4")]
    pub port: u32,
    #[prost(bytes = "vec", tag = "5")]
    pub dcid: ::prost::alloc::vec::Vec<u8>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct IpAddress {
    #[prost(oneof = "ip_address::Address", tags = "1, 2")]
    pub address: ::core::option::Option<ip_address::Address>,
}
/// Nested message and enum types in `IpAddress`.
pub mod ip_address {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Address {
        #[prost(bytes, tag = "1")]
        V4(::prost::alloc::vec::Vec<u8>),
        #[prost(bytes, tag = "2")]
        V6(::prost::alloc::vec::Vec<u8>),
    }
}
