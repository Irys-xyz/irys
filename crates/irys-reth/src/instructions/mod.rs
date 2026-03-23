// Both modules are consumed when the Sprite hardfork instruction table is wired (Task 4).
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "consumed once the Sprite hardfork handler table is wired"
    )
)]
pub(crate) mod pd_return_marker;
#[expect(dead_code, reason = "consumed once the Sprite hardfork handler table is wired")]
pub(crate) mod returndatacopy;
