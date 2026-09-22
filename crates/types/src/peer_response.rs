/// Lengths only. The rejected chunk is not returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseBodyTooLarge {
    pub accumulated: u64,
    pub chunk_len: u64,
    pub cap: u64,
}

/// `Ok(new_total)` when `accumulated + chunk_len` fits in `u64` and is `<= cap`.
/// Overflow and a sum past `cap` are the same error.
pub fn accumulate_response_bytes(
    accumulated: u64,
    chunk_len: u64,
    cap: u64,
) -> Result<u64, ResponseBodyTooLarge> {
    let Some(total) = accumulated.checked_add(chunk_len) else {
        return Err(ResponseBodyTooLarge {
            accumulated,
            chunk_len,
            cap,
        });
    };
    if total > cap {
        Err(ResponseBodyTooLarge {
            accumulated,
            chunk_len,
            cap,
        })
    } else {
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::accumulate_response_bytes;
    use proptest::prelude::*;

    #[test]
    fn exact_cap_is_accepted_and_one_past_is_rejected() {
        assert_eq!(accumulate_response_bytes(0, 4, 4).unwrap(), 4);
        assert!(accumulate_response_bytes(4, 1, 4).is_err());
        assert!(accumulate_response_bytes(u64::MAX, 1, u64::MAX).is_err());
    }

    proptest! {
        #[test]
        fn sums_inside_the_cap_succeed_and_the_crossing_chunk_fails(
            chunks in prop::collection::vec(0_u64..10_000, 0..40),
            cap in 0_u64..500_000_u64,
        ) {
            let mut total = 0_u64;
            for chunk in chunks {
                match accumulate_response_bytes(total, chunk, cap) {
                    Ok(next) => {
                        prop_assert!(next <= cap);
                        prop_assert_eq!(next, total.checked_add(chunk).unwrap());
                        total = next;
                    }
                    Err(error) => {
                        prop_assert!(
                            total.checked_add(chunk).map(|sum| sum > cap).unwrap_or(true)
                        );
                        prop_assert_eq!(error.accumulated, total);
                        prop_assert_eq!(error.chunk_len, chunk);
                        break;
                    }
                }
            }
        }
    }
}
