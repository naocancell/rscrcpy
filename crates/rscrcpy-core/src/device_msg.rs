//! Messages sent by the device on the control socket.

use std::io::{self, Read};

const TYPE_CLIPBOARD: u8 = 0;
const TYPE_ACK_CLIPBOARD: u8 = 1;
const TYPE_UHID_OUTPUT: u8 = 2;
const MAX_CLIPBOARD_LENGTH: usize = 1 << 18;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceMsg {
    /// The device clipboard changed (or was requested with GET_CLIPBOARD).
    Clipboard(String),
    /// Acknowledges a SET_CLIPBOARD with a non-zero sequence.
    AckClipboard(u64),
    UhidOutput {
        id: u16,
        data: Vec<u8>,
    },
}

fn read_n<R: Read, const N: usize>(r: &mut R) -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    r.read_exact(&mut b)?;
    Ok(b)
}

pub fn read_device_msg(r: &mut impl Read) -> io::Result<DeviceMsg> {
    let [ty] = read_n::<_, 1>(r)?;
    match ty {
        TYPE_CLIPBOARD => {
            let len = u32::from_be_bytes(read_n(r)?) as usize;
            if len > MAX_CLIPBOARD_LENGTH {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "clipboard message too large"));
            }
            let mut text = vec![0u8; len];
            r.read_exact(&mut text)?;
            Ok(DeviceMsg::Clipboard(String::from_utf8_lossy(&text).into_owned()))
        }
        TYPE_ACK_CLIPBOARD => Ok(DeviceMsg::AckClipboard(u64::from_be_bytes(read_n(r)?))),
        TYPE_UHID_OUTPUT => {
            let id = u16::from_be_bytes(read_n(r)?);
            let len = u16::from_be_bytes(read_n(r)?) as usize;
            let mut data = vec![0u8; len];
            r.read_exact(&mut data)?;
            Ok(DeviceMsg::UhidOutput { id, data })
        }
        other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown device message type {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_messages() {
        let mut input: &[u8] = &[
            0, 0, 0, 0, 3, b'a', b'b', b'c', // clipboard "abc"
            1, 1, 2, 3, 4, 5, 6, 7, 8, // ack
            2, 0, 42, 0, 2, 0xab, 0xcd, // uhid output
        ];
        assert_eq!(read_device_msg(&mut input).unwrap(), DeviceMsg::Clipboard("abc".into()));
        assert_eq!(read_device_msg(&mut input).unwrap(), DeviceMsg::AckClipboard(0x0102030405060708));
        assert_eq!(read_device_msg(&mut input).unwrap(), DeviceMsg::UhidOutput { id: 42, data: vec![0xab, 0xcd] });
        assert!(read_device_msg(&mut input).is_err());
    }
}
