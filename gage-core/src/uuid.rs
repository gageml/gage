/// Crockford base32 alphabet (lowercase). No I, L, O, U.
const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Generate a new random ID: 128 bits of UUIDv4 entropy rendered as 26 chars
/// of Crockford base32 (lowercase). Compact, prefix-friendly, and visually
/// distinct from hex UUIDs used elsewhere (e.g. Claude session IDs).
pub fn new_uuid() -> String {
    encode_crockford(uuid::Uuid::new_v4().as_bytes())
}

pub fn short_uuid(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn encode_crockford(bytes: &[u8; 16]) -> String {
    let n = u128::from_be_bytes(*bytes);
    let mut out = [0u8; 26];
    // 26 chars * 5 bits = 130 bits; the top two bits are zero-padded.
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = 5 * (25 - i);
        *slot = *CROCKFORD.get(((n >> shift) & 0x1f) as usize).unwrap();
    }
    String::from_utf8(out.to_vec()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_and_alphabet() {
        let id = new_uuid();
        assert_eq!(id.len(), 26);
        assert!(id.bytes().all(|b| CROCKFORD.contains(&b)));
    }

    #[test]
    fn encoding_is_deterministic() {
        let bytes = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ];
        let a = encode_crockford(&bytes);
        let b = encode_crockford(&bytes);
        assert_eq!(a, b);
        assert_eq!(a.len(), 26);
    }
}
