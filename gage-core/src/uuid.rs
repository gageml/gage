/// Crockford base32 alphabet (lowercase). No I, L, O, U.
const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Generate a new random ID: 128 bits of UUIDv4 entropy rendered as 26 chars
/// of Crockford base32 (lowercase). Compact, prefix-friendly, and visually
/// distinct from hex UUIDs used elsewhere (e.g. Claude session IDs).
pub fn new_uuid() -> String {
    encode_crockford(uuid::Uuid::new_v4().as_bytes())
}

/// Namespace UUID for Gage-derived object IDs. A fixed random constant;
/// changing it would relabel every derived object.
const GAGE_ID_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x7d, 0xb2, 0x1a, 0x0e, 0x4f, 0x3c, 0x4a, 0x87, 0xa1, 0x0e, 0x9c, 0x2b, 0x5d, 0xf7, 0x1a, 0x64,
]);

/// Derive a stable 26-char Crockford ID from a key. Used for object
/// types whose IDs are computed from external identifiers (sessions,
/// contexts) so the same input always maps to the same object.
pub fn derive_id(key: &str) -> String {
    encode_crockford(uuid::Uuid::new_v5(&GAGE_ID_NAMESPACE, key.as_bytes()).as_bytes())
}

pub fn short_uuid(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Generate a ULID: 26 chars of Crockford base32 whose first 10 chars
/// encode the current time in milliseconds and whose last 16 are
/// random. Sorting ULIDs as strings sorts them by creation time, so a
/// tree listing of ULID-named entries is chronological.
pub fn new_ulid() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let random = uuid::Uuid::new_v4().into_bytes();
    let mut bytes = [0u8; 16];
    // 48-bit timestamp, then 80 random bits
    bytes[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    bytes[6..].copy_from_slice(&random[6..]);
    encode_crockford(&bytes)
}

/// The millisecond timestamp encoded in a ULID's first 10 chars, or
/// `None` when the value is not 26 chars of the Crockford alphabet.
pub fn ulid_timestamp_ms(ulid: &str) -> Option<i64> {
    if ulid.len() != 26 {
        return None;
    }
    let mut n: u128 = 0;
    for b in ulid.bytes() {
        let digit = CROCKFORD.iter().position(|c| *c == b)? as u128;
        n = (n << 5) | digit;
    }
    // The top 2 of the 130 bits are padding; the next 48 are the time
    Some((n >> 80) as i64)
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
    fn ulid_sorts_by_time_and_decodes_its_timestamp() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let a = new_ulid();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_ulid();
        assert_eq!(a.len(), 26);
        assert!(a.bytes().all(|b| CROCKFORD.contains(&b)));
        assert!(a < b, "{a} {b}");
        let ts = ulid_timestamp_ms(&a).unwrap();
        assert!(ts >= before && ts <= before + 10_000, "{ts} vs {before}");
        assert_eq!(ulid_timestamp_ms("short"), None);
        assert_eq!(ulid_timestamp_ms(&"i".repeat(26)), None);
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
