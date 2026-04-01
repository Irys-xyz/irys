#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed = 0,
    Open = 1,
    HalfOpen = 2,
}

impl CircuitState {
    pub(crate) fn from_u8_failsafe(value: u8) -> Self {
        match Self::try_from(value) {
            Ok(state) => state,
            Err(invalid) => {
                tracing::error!(
                    "Invalid CircuitState byte value: {}. Defaulting to Open for safety.",
                    invalid
                );
                Self::Open
            }
        }
    }
}

impl TryFrom<u8> for CircuitState {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Closed),
            1 => Ok(Self::Open),
            2 => Ok(Self::HalfOpen),
            invalid => Err(invalid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn from_u8_failsafe_never_panics(value: u8) {
            let state = CircuitState::from_u8_failsafe(value);
            match value {
                0 => prop_assert_eq!(state, CircuitState::Closed),
                1 => prop_assert_eq!(state, CircuitState::Open),
                2 => prop_assert_eq!(state, CircuitState::HalfOpen),
                _ => prop_assert_eq!(state, CircuitState::Open, "invalid values default to Open"),
            }
        }

        #[test]
        fn try_from_roundtrip_for_valid_values(value in 0_u8..=2) {
            let state = CircuitState::try_from(value).unwrap();
            prop_assert_eq!(state as u8, value);
        }

        #[test]
        fn try_from_rejects_invalid_values(value in 3_u8..=u8::MAX) {
            let result = CircuitState::try_from(value);
            prop_assert!(result.is_err());
            prop_assert_eq!(result.unwrap_err(), value);
        }
    }
}
