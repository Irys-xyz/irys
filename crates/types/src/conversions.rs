use crate::Address;
use std::str::FromStr as _;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum AddressParseError {
    #[error("Invalid address format: {0}")]
    InvalidFormat(String),
}

/// Parse an Address from a string
pub fn parse_address(s: &str) -> Result<Address, AddressParseError> {
    Address::from_str(s)
        .map_err(|e| AddressParseError::InvalidFormat(e.to_string()))
}