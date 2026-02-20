use serde::{Deserialize, Serialize};
use std::fmt;

/// Time-ordered unique request identifier (UUID v7 layout).
///
/// Uses 48-bit millisecond timestamp + 80 bits of randomness with
/// UUID version 7 and variant bits set, giving sortable, globally-unique IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId([u8; 16]);

impl RequestId {
    pub fn new() -> Self {
        use rand::Rng as _;
        use std::time::{SystemTime, UNIX_EPOCH};

        let mut bytes = [0_u8; 16];

        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // Bytes 0-5: 48-bit timestamp (big-endian)
        bytes[0] = (ts_ms >> 40) as u8;
        bytes[1] = (ts_ms >> 32) as u8;
        bytes[2] = (ts_ms >> 24) as u8;
        bytes[3] = (ts_ms >> 16) as u8;
        bytes[4] = (ts_ms >> 8) as u8;
        bytes[5] = ts_ms as u8;

        // Bytes 6-15: random
        let mut rng = rand::thread_rng();
        rng.fill(&mut bytes[6..]);

        // Set version (4 bits in byte 6 high nibble) to 0b0111 (version 7)
        bytes[6] = (bytes[6] & 0x0F) | 0x70;
        // Set variant (2 bits in byte 8 high bits) to 0b10 (RFC 9562)
        bytes[8] = (bytes[8] & 0x3F) | 0x80;

        Self(bytes)
    }
}

impl Default for RequestId {
    /// Generates a new unique `RequestId` (equivalent to `RequestId::new()`).
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3],
            b[4], b[5],
            b[6], b[7],
            b[8], b[9],
            b[10], b[11], b[12], b[13], b[14], b[15],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_format_is_uuid() {
        let id = RequestId::new();
        let s = id.to_string();
        assert_eq!(s.len(), 36);
        assert_eq!(&s[8..9], "-");
        assert_eq!(&s[13..14], "-");
        assert_eq!(&s[18..19], "-");
        assert_eq!(&s[23..24], "-");
    }

    #[test]
    fn version_and_variant_bits() {
        let id = RequestId::new();
        // Version 7: byte 6 high nibble == 0x7
        assert_eq!(id.0[6] >> 4, 0x7);
        // Variant: byte 8 top 2 bits == 0b10
        assert_eq!(id.0[8] >> 6, 0b10);
    }

    #[test]
    fn ids_are_time_ordered() {
        let id1 = RequestId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id2 = RequestId::new();
        // Compare first 6 bytes (timestamp)
        assert!(id1.0[..6] <= id2.0[..6]);
    }

    #[test]
    fn serde_roundtrip() {
        let id = RequestId::new();
        let json = serde_json::to_string(&id).unwrap();
        let deserialized: RequestId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, deserialized);
    }
}
