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
