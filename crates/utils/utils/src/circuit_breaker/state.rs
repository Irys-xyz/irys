#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed = 0,
    Open = 1,
    HalfOpen = 2,
}

impl CircuitState {
    fn try_from_u8(value: u8) -> Result<Self, u8> {
        match value {
            0 => Ok(Self::Closed),
            1 => Ok(Self::Open),
            2 => Ok(Self::HalfOpen),
            invalid => Err(invalid),
        }
    }
}

impl From<u8> for CircuitState {
    fn from(value: u8) -> Self {
        match Self::try_from_u8(value) {
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
