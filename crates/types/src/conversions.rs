use crate::{Address, U256};
use std::str::FromStr as _;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum AddressParseError {
    #[error("Invalid address format: {0}")]
    InvalidFormat(String),
}

/// Parse an Address from a string
pub fn parse_address(s: &str) -> Result<Address, AddressParseError> {
    Address::from_str(s).map_err(|e| AddressParseError::InvalidFormat(e.to_string()))
}

/// Convert little-endian bytes to U256
pub fn u256_from_le_bytes(bytes: &[u8]) -> U256 {
    U256::from_little_endian(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::*;

    #[fixture]
    fn valid_addresses() -> Vec<&'static str> {
        vec![
            "0x1234567890abcdef1234567890abcdef12345678",
            "0xABCDEF1234567890ABCDEF1234567890ABCDEF12",
            "0x0000000000000000000000000000000000000000",
        ]
    }

    #[fixture]
    fn invalid_addresses() -> Vec<&'static str> {
        vec![
            "not_an_address",
            "0xinvalid",
            "",
            "0x123",                                       // too short
            "0x1234567890abcdef1234567890abcdef123456789", // too long
        ]
    }

    mod address_parsing_tests {
        use super::*;

        #[rstest]
        fn should_parse_valid_addresses(valid_addresses: Vec<&str>) {
            for address_str in valid_addresses {
                let result = parse_address(address_str);
                assert!(
                    result.is_ok(),
                    "Failed to parse valid address: {address_str}"
                );
            }
        }

        #[rstest]
        fn should_reject_invalid_addresses(invalid_addresses: Vec<&str>) {
            for address_str in invalid_addresses {
                let result = parse_address(address_str);
                assert!(
                    result.is_err(),
                    "Should have failed to parse invalid address: {address_str}"
                );
                assert!(matches!(
                    result.unwrap_err(),
                    AddressParseError::InvalidFormat(_)
                ));
            }
        }

        #[rstest]
        #[case("0x1234567890abcdef1234567890abcdef12345678")]
        #[case("0xABCDEF1234567890ABCDEF1234567890ABCDEF12")]
        fn should_parse_specific_valid_addresses(#[case] address: &str) {
            let result = parse_address(address);
            assert!(result.is_ok());
        }

        #[rstest]
        #[case("invalid_address")]
        #[case("0xinvalid")]
        #[case("")]
        fn should_reject_specific_invalid_addresses(#[case] address: &str) {
            let result = parse_address(address);
            assert!(result.is_err());
            match result.unwrap_err() {
                AddressParseError::InvalidFormat(_) => {}
            }
        }
    }

    #[test]
    fn should_parse_zero() {
        assert_eq!(u256_from_le_bytes(&[]), U256::from(0));
        assert_eq!(u256_from_le_bytes(&[0_u8; 32]), U256::from(0));
    }

    #[test]
    fn should_parse_single_byte() {
        assert_eq!(u256_from_le_bytes(&[1]), U256::from(1));
        assert_eq!(u256_from_le_bytes(&[0xff]), U256::from(255_u64));
    }

    #[test]
    fn should_respect_little_endian() {
        // 0x1234 in LE is [0x34, 0x12]
        assert_eq!(u256_from_le_bytes(&[0x34, 0x12]), U256::from(0x1234_u64));
        // 0x0001020304 in LE is [0x04, 0x03, 0x02, 0x01, 0x00]
        assert_eq!(
            u256_from_le_bytes(&[0x04, 0x03, 0x02, 0x01, 0x00]),
            U256::from(0x0000_0102_0304_u64)
        );
    }

    #[test]
    fn should_handle_longer_inputs() {
        // Ensure it handles lengths > 8 bytes without panic and yields non-zero for non-zero input
        let bytes = vec![7_u8; 17];
        let val = u256_from_le_bytes(&bytes);
        assert!(val > U256::from(0));
    }
}
