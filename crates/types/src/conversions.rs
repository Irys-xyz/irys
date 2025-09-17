use crate::Address;
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
            "0x123", // too short
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
                    "Failed to parse valid address: {}",
                    address_str
                );
            }
        }

        #[rstest]
        fn should_reject_invalid_addresses(invalid_addresses: Vec<&str>) {
            for address_str in invalid_addresses {
                let result = parse_address(address_str);
                assert!(
                    result.is_err(),
                    "Should have failed to parse invalid address: {}",
                    address_str
                );
                assert!(matches!(result.unwrap_err(), AddressParseError::InvalidFormat(_)));
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
}
