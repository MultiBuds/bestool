use crate::beslink::{BESLinkError, BES_SYNC, FLASH_BUFFER_SIZE};
use serialport::SerialPort;
use std::convert::TryFrom;
use std::io::ErrorKind::TimedOut;
use std::io::{Read, Write};
use std::time::Duration;
use tracing::{debug, error, warn};

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum MessageTypes {
    Sync = 0x50, // Seems to be used at boot for locking with ROM
    FlashRead = 0x03,
    StartProgrammer = 0x53,
    ProgrammerRunning = 0x54,
    ProgrammerStart = 0x55,
    ProgrammerInit = 0x60,
    FlashCommand = 0x65, // Suspect used to push extra commands to flash controller/chip/die
    EraseBurnStart = 0x61,
    FlashBurnData = 0x62,
}
impl TryFrom<u8> for MessageTypes {
    type Error = ();

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            x if x == MessageTypes::Sync as u8 => Ok(MessageTypes::Sync),
            x if x == MessageTypes::StartProgrammer as u8 => Ok(MessageTypes::StartProgrammer),
            x if x == MessageTypes::ProgrammerRunning as u8 => Ok(MessageTypes::ProgrammerRunning),
            x if x == MessageTypes::ProgrammerInit as u8 => Ok(MessageTypes::ProgrammerInit),
            x if x == MessageTypes::FlashCommand as u8 => Ok(MessageTypes::FlashCommand),
            x if x == MessageTypes::EraseBurnStart as u8 => Ok(MessageTypes::EraseBurnStart),
            x if x == MessageTypes::FlashBurnData as u8 => Ok(MessageTypes::FlashBurnData),
            x if x == MessageTypes::FlashRead as u8 => Ok(MessageTypes::FlashRead),
            _ => Err(()),
        }
    }
}
#[derive(Debug, PartialEq, Clone)]
pub struct BesMessage {
    pub sync: u8,
    pub type1: MessageTypes,
    pub payload: Vec<u8>,
    pub checksum: u8,
}

impl BesMessage {
    pub fn to_vec(&self) -> Vec<u8> {
        let mut result: Vec<u8> = vec![];
        result.push(self.sync);
        result.push(self.type1 as u8);
        result.append(&mut self.payload.clone());
        result.push(self.checksum);
        return result;
    }
    pub fn set_checksum(&mut self) {
        let mut v = self.to_vec();
        v.pop();
        self.checksum = calculate_message_checksum(&v);
    }
}

impl From<Vec<u8>> for BesMessage {
    fn from(d: Vec<u8>) -> Self {
        let mut msg = BesMessage {
            sync: d[0],
            type1: MessageTypes::Sync,
            payload: vec![],
            checksum: d[d.len() - 1],
        };

        match d[1].try_into() {
            Ok(type1) => msg.type1 = type1,
            Err(_) => {
                println!("Unknown packet type 0x{:02X}", d[1]);
            }
        };

        msg.payload = d[1..d.len() - 1].to_vec();

        return msg;
    }
}

pub fn send_message(serial_port: &mut Box<dyn SerialPort>, msg: BesMessage) -> std::io::Result<()> {
    let packet = msg.to_vec();
    return match serial_port.write_all(packet.as_slice()) {
        Ok(_) => {
            debug!("Wrote {} bytes", packet.len());
            let _ = serial_port.flush();
            Ok(())
        }
        Err(e) => {
            error!("Writing to port raised {:?}", e);
            Err(e)
        }
    };
}
pub fn read_message_with_trailing_data(
    serial_port: &mut Box<dyn SerialPort>,
    expected_data_len: usize,
) -> Result<(BesMessage, Vec<u8>), BESLinkError> {
    //First read the packet; then read the expected_raw_bytes from the uart
    //TODO for now assuming the 0x03 code for response

    let response = read_message(serial_port)?;
    if response.type1 != MessageTypes::FlashRead {
        error!("Bad packet type: {:?}", response.type1);
        return Err(BESLinkError::InvalidArgs);
    }
    let mut packet: Vec<u8> = vec![];
    let mut buffer: [u8; FLASH_BUFFER_SIZE] = [0; FLASH_BUFFER_SIZE];

    while packet.len() < expected_data_len {
        match serial_port.read(&mut buffer) {
            Ok(n) => {
                if n > 0 {
                    packet.extend(&buffer[0..n]);
                } else {
                    warn!("Stalled packet");
                }
            }
            Err(e) => {
                if e.kind() != TimedOut {
                    println!("Error reading packet header {:?}", e);
                    return Err(BESLinkError::from(e));
                }
            }
        }
    }
    return Ok((response, packet));
}
pub fn read_message(serial_port: &mut Box<dyn SerialPort>) -> Result<BesMessage, BESLinkError> {
    //
    let mut packet: Vec<u8> = vec![];
    let mut packet_len: usize = 3; //Start expectations at the minimum
    let mut buffer: [u8; 1] = [0; 1];

    while packet.len() < packet_len {
        match serial_port.read(&mut buffer) {
            Ok(n) => {
                if n == 1 {
                    // Only grab if actual data
                    if !(packet.len() == 0 && buffer[0] != BES_SYNC) {
                        packet.push(buffer[0]);
                    }
                }
            }
            Err(e) => {
                if e.kind() != TimedOut {
                    println!("Error reading packet header {:?}", e);
                    return Err(BESLinkError::from(e));
                }
            }
        }
        if packet.len() == 3 && packet_len == 3 {
            //Check actual packet length
            packet_len = decode_message_length(&packet) as usize;
            debug!("Got packet len lookup {} for {}", packet_len, packet[1])
        }
        //TODO timeout
    }
    std::thread::sleep(Duration::from_millis(5));

    return match validate_packet_checksum(&packet) {
        Ok(_) => Ok(BesMessage::from(packet)),
        Err(e) => Err(e),
    };
}
pub fn validate_packet_checksum(packet: &Vec<u8>) -> Result<(), BESLinkError> {
    let mut inner_packet = packet.clone();
    let _ = inner_packet.pop();
    let checksum = calculate_message_checksum(&inner_packet);
    if checksum == packet[packet.len() - 1] {
        return Ok(());
    }
    let e = BESLinkError::BadChecksumError {
        failed_packet: packet.clone(),
        got: packet[packet.len() - 1],
        wanted: checksum,
    };
    warn!("Bad Checksum!! {:?}", e);
    return Err(e);
}
pub fn calculate_message_checksum(packet: &Vec<u8>) -> u8 {
    let mut sum: u32 = 0;
    for b in packet {
        sum += *b as u32;
        sum = sum & 0xFF;
    }
    return (0xFF - sum) as u8;
}
fn decode_message_length(packet: &Vec<u8>) -> u16 {
    if packet.len() < 3 {
        return 3; // fail safe
    }
    let packet_id1 = packet[1];
    let packet_id2 = packet[2];

    return match packet_id1.try_into() {
        Ok(type1) => match type1 {
            MessageTypes::Sync => 8,
            MessageTypes::StartProgrammer => 6,
            MessageTypes::ProgrammerRunning => 6,
            MessageTypes::ProgrammerInit => 11,
            MessageTypes::FlashCommand => {
                if packet_id2 == 2 {
                    return 9;
                } else if packet_id2 == 0x08 {
                    return 6;
                }
                return 22;
            }
            MessageTypes::EraseBurnStart => 6,
            MessageTypes::FlashBurnData => 8,
            MessageTypes::ProgrammerStart => 6,
            MessageTypes::FlashRead => {
                return 6;
            }
        },
        Err(_) => {
            println!(
                "Unknown packet len 0x{:02X}/0x{:02X}",
                packet_id1, packet_id2
            );
            return 3;
        }
    };
}

#[cfg(test)]
mod tests {
    use crate::beslink::message::calculate_message_checksum;

    #[test]
    fn test_calculate_packet_checksum() {
        //make fake port it can write to
        let test_messages: Vec<Vec<u8>> = vec![
            vec![0xBE, 0x50, 0x00, 0x03, 0x00, 0x00, 0x01, 0xED],
            vec![0xBE, 0x50, 0x00, 0x01, 0x01, 0xEF],
            vec![0xBE, 0x53, 0x00, 0x01, 0x00, 0xED],
            vec![0xBE, 0x65, 0x02, 0x01, 0x11, 0xC8],
            vec![0xBE, 0x65, 0x03, 0x01, 0x12, 0xC6],
            vec![
                0xBE, 0x62, 0xC1, 0x0B, 0x00, 0x80, 0x00, 0x00, 0xAB, 0x77, 0x7F, 0xF4, 0x00, 0x00,
                0x00, 0xFE,
            ],
            vec![
                0xBE, 0x62, 0xC2, 0x0B, 0x00, 0x80, 0x00, 0x00, 0x34, 0x90, 0x61, 0xF9, 0x01, 0x00,
                0x00, 0x73,
            ],
            vec![
                0xBE, 0x61, 0x07, 0x0C, 0x00, 0x00, 0x00, 0x3C, 0x00, 0x00, 0x0D, 0x00, 0x00, 0x80,
                0x00, 0x00, 0x04,
            ],
            vec![
                0xBE, 0x03, 0x06, 0x08, 0x00, 0xF0, 0x0F, 0x3C, 0x00, 0x10, 0x00, 0x00, 0xE5,
            ],
            vec![
                0xBE, 0x03, 0x05, 0x08, 0x00, 0xE0, 0x0F, 0x3C, 0x00, 0x10, 0x00, 0x00, 0xF6,
            ],
        ];
        for mut v in test_messages {
            let old_checksum = v.pop().unwrap();
            let new_checksum = calculate_message_checksum(&v);
            assert_eq!(old_checksum, new_checksum);
        }
    }
}
