#![no_std]

use heapless::{String, Vec};
use serde::{Deserialize, Serialize};

pub const OTA_DATA_SIZE: usize = 512;

#[derive(Debug, Serialize, Deserialize)]
pub enum Packet {
    Message(String<64>),
    OtaPacket(OtaPacket),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OtaPacket {
    pub num: u32,
    pub total: u32,
    pub data: Vec<u8, OTA_DATA_SIZE>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Ack {
    pub num: u32,
}
