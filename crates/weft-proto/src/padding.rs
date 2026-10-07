use crate::packet::PacketError;

pub const BLOCK: usize = 16;

pub fn padded_len(len: usize) -> usize {
    len.next_multiple_of(BLOCK)
}

/// Returns `None` for a keepalive, otherwise the length of the IP packet at the start of `plaintext`.
pub fn ip_packet_len(plaintext: &[u8]) -> Result<Option<usize>, PacketError> {
    let Some(&first) = plaintext.first() else {
        return Ok(None);
    };
    let (len, min) = match first >> 4 {
        0 => return Ok(None),
        4 if plaintext.len() >= 20 => (usize::from(u16::from_be_bytes([plaintext[2], plaintext[3]])), 20),
        6 if plaintext.len() >= 40 => (40 + usize::from(u16::from_be_bytes([plaintext[4], plaintext[5]])), 40),
        _ => return Err(PacketError::InvalidPayload),
    };
    if len < min || len > plaintext.len() {
        return Err(PacketError::InvalidPayload);
    }
    Ok(Some(len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipv4(total: u16) -> Vec<u8> {
        let mut packet = vec![0; usize::from(total)];
        packet[0] = 0x45;
        packet[2..4].copy_from_slice(&total.to_be_bytes());
        packet
    }

    fn ipv6(payload: u16) -> Vec<u8> {
        let mut packet = vec![0; 40 + usize::from(payload)];
        packet[0] = 0x60;
        packet[4..6].copy_from_slice(&payload.to_be_bytes());
        packet
    }

    #[test]
    fn padding() {
        assert_eq!(padded_len(0), 0);
        assert_eq!(padded_len(1), 16);
        assert_eq!(padded_len(16), 16);
        assert_eq!(padded_len(1281), 1296);
    }

    #[test]
    fn keepalive() {
        assert_eq!(ip_packet_len(&[]), Ok(None));
        assert_eq!(ip_packet_len(&[0; 48]), Ok(None));
    }

    #[test]
    fn ip_packets_with_padding() {
        let mut packet = ipv4(61);
        packet.resize(padded_len(61), 0);
        assert_eq!(ip_packet_len(&packet), Ok(Some(61)));

        let mut packet = ipv6(8);
        packet.resize(64, 0);
        assert_eq!(ip_packet_len(&packet), Ok(Some(48)));
    }

    #[test]
    fn malformed() {
        let mut packet = ipv4(40);
        packet.truncate(30);
        assert_eq!(ip_packet_len(&packet), Err(PacketError::InvalidPayload));
        assert_eq!(ip_packet_len(&ipv4(20)[..19]), Err(PacketError::InvalidPayload));
        let mut packet = ipv4(20);
        packet[2..4].copy_from_slice(&10u16.to_be_bytes());
        assert_eq!(ip_packet_len(&packet), Err(PacketError::InvalidPayload));
        assert_eq!(ip_packet_len(&[0x50; 32]), Err(PacketError::InvalidPayload));
    }
}
