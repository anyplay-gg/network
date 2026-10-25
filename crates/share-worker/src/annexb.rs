//! Annex B, the form of H.264 the share engine cuts into datagrams: NAL units after start codes.

/// Position of the first start code (`00 00 01`) at or after `from`.
pub fn find_start(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|p| p + from)
}

/// The types of the NAL units in an Annex B stream.
pub fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut at = find_start(data, 0);
    while let Some(start) = at {
        if let Some(byte) = data.get(start + 3) {
            types.push(byte & 0x1f);
        }
        at = find_start(data, start + 3);
    }
    types
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_types_are_read_from_annex_b() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9,
        ];
        assert_eq!(nal_types(&data), vec![7, 8, 5]);
        assert!(nal_types(&[1, 2, 3]).is_empty());
    }
}
