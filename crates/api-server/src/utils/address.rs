use crate::error::ApiError;
use base58::FromBase58 as _;
use irys_types::Address;
use std::str::FromStr as _;

pub fn parse_address(address_str: &str) -> Result<Address, ApiError> {
    if let Ok(decoded) = address_str.from_base58() {
        if let Ok(arr) = TryInto::<[u8; 20]>::try_into(decoded.as_slice()) {
            return Ok(Address::from(&arr));
        }
    }

    Address::from_str(address_str).map_err(|e| ApiError::InvalidAddressFormat {
        address: address_str.to_string(),
        error: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("0x0000000000000000000000000000000000000001", true)]
    #[case("0x742d35cc6634c0532925a3b844bc9e7595f0beb0", true)]
    #[case("not_an_address", false)]
    #[case("0x", false)]
    #[case("0xinvalid", false)]
    fn test_parse_address(#[case] input: &str, #[case] should_succeed: bool) {
        let result = parse_address(input);
        assert_eq!(result.is_ok(), should_succeed);
    }
}
