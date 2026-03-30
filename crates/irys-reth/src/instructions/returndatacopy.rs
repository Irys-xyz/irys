//! PD-aware `RETURNDATACOPY` replacement handler.
//!
//! When the current frame's `return_data` buffer is the exact allocation
//! returned by the PD precompile (detected via [`is_current_pd_return_marker`]),
//! memory expansion gas is skipped. All other semantics — stack pop ordering,
//! bounds checks, copy gas, zero-length early return — match the upstream
//! `RETURNDATACOPY` implementation in revm-interpreter 32.0.0.

use revm::context_interface::{Host, cfg::GasParams};
use revm::interpreter::interpreter_types::{
    MemoryTr as _, ReturnData as _, RuntimeFlag as _, StackTr as _,
};
use revm::interpreter::{
    InstructionContext, InstructionResult, Interpreter, InterpreterTypes, num_words,
};
use revm::primitives::{U256, hardfork::SpecId};

use crate::instructions::pd_return_marker::is_current_pd_return_marker;

// ---------------------------------------------------------------------------
// Helper functions (replace revm internal macros with plain Rust)
// ---------------------------------------------------------------------------

/// Returns `false` and halts the interpreter if the Byzantium spec is not active.
fn ensure_byzantium<W: InterpreterTypes>(interpreter: &mut Interpreter<W>) -> bool {
    if !interpreter
        .runtime_flag
        .spec_id()
        .is_enabled_in(SpecId::BYZANTIUM)
    {
        interpreter.halt_not_activated();
        return false;
    }
    true
}

/// Pops three values from the stack.
/// Returns `None` and halts with stack-underflow on failure.
fn pop3_or_underflow<W: InterpreterTypes>(interpreter: &mut Interpreter<W>) -> Option<[U256; 3]> {
    let popped = interpreter.stack.popn::<3>();
    if popped.is_none() {
        interpreter.halt_underflow();
    }
    popped
}

/// Converts a `U256` to `usize` with the same semantics as revm's
/// `as_usize_or_fail!` macro: values that exceed `usize::MAX` halt
/// with `InvalidOperandOOG`.
fn u256_to_usize_or_fail<W: InterpreterTypes>(
    interpreter: &mut Interpreter<W>,
    value: U256,
) -> Option<usize> {
    let limbs = value.as_limbs();
    #[expect(
        clippy::as_conversions,
        reason = "matching revm's as_usize_or_fail semantics"
    )]
    if (limbs[0] > usize::MAX as u64) || (limbs[1] != 0) || (limbs[2] != 0) || (limbs[3] != 0) {
        interpreter.halt(InstructionResult::InvalidOperandOOG);
        return None;
    }
    #[expect(
        clippy::as_conversions,
        reason = "u64 fits in usize after the check above"
    )]
    Some(limbs[0] as usize)
}

/// Saturating conversion from `U256` to `usize`.
/// Values exceeding `usize::MAX` clamp to `usize::MAX`.
fn u256_to_usize_saturated(value: U256) -> usize {
    let limbs = value.as_limbs();
    let as_u64 = if (limbs[1] == 0) & (limbs[2] == 0) & (limbs[3] == 0) {
        limbs[0]
    } else {
        u64::MAX
    };
    usize::try_from(as_u64).unwrap_or(usize::MAX)
}

/// Charges `cost` gas and halts with OOG on failure.
/// Returns `false` if the interpreter has been halted.
fn charge_cost_or_oog<W: InterpreterTypes>(interpreter: &mut Interpreter<W>, cost: u64) -> bool {
    if !interpreter.gas.record_cost(cost) {
        interpreter.halt_oog();
        return false;
    }
    true
}

/// Resizes the interpreter's memory WITHOUT charging the memory-expansion gas
/// delta.
///
/// This is the core of the PD memory expansion gas elimination feature.
/// The memory accounting (`MemoryGas::words_num` and `expansion_cost`) is
/// still updated so that subsequent (non-PD) memory operations compute their
/// costs relative to the correct high-water mark. Only the gas deduction is
/// skipped.
fn resize_memory_without_gas<W: InterpreterTypes>(
    interpreter: &mut Interpreter<W>,
    gas_params: &GasParams,
    offset: usize,
    len: usize,
) -> bool {
    let new_num_words = num_words(offset.saturating_add(len));
    if new_num_words > interpreter.gas.memory().words_num {
        let new_cost = gas_params.memory_cost(new_num_words);
        // Intentionally discard the delta — this is the gas we are NOT charging.
        let _delta = interpreter
            .gas
            .memory_mut()
            .set_words_num(new_num_words, new_cost)
            .expect("new_num_words > current words_num implies Some(delta)");

        let _ = interpreter.memory.resize(new_num_words * 32);
    }

    true
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// PD-aware `RETURNDATACOPY` handler.
///
/// Static gas cost of 3 is charged by the instruction table; this handler only
/// deals with dynamic gas (copy cost + memory expansion).
///
/// When the interpreter's current `return_data` buffer is the same allocation
/// that was stored by the PD precompile (identity check via
/// [`is_current_pd_return_marker`]), memory expansion gas is waived. All other
/// gas and semantic behaviour is identical to upstream `RETURNDATACOPY`.
/// Compute the EVM memory expansion cost for `num_words` words.
/// `cost = 3 * num_words + num_words² / 512`
#[cfg(test)]
fn expected_memory_cost(num_words: usize) -> u64 {
    let w = num_words as u64;
    3 * w + w * w / 512
}

pub(crate) fn irys_returndatacopy<W: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, W>,
) {
    // 1. Check BYZANTIUM activation.
    if !ensure_byzantium(context.interpreter) {
        return;
    }

    // 2. Pop [memory_offset, offset, len] from the stack.
    let Some([memory_offset, offset, len_word]) = pop3_or_underflow(context.interpreter) else {
        return;
    };

    // 3. Convert len with as_usize_or_fail semantics.
    let Some(len) = u256_to_usize_or_fail(context.interpreter, len_word) else {
        return;
    };

    // 4. Convert data_offset with saturating semantics.
    let data_offset = u256_to_usize_saturated(offset);

    // 5. Bounds-check: data_end must not exceed return_data buffer length.
    let data_end = data_offset.saturating_add(len);
    if data_end > context.interpreter.return_data.buffer().len() {
        context.interpreter.halt(InstructionResult::OutOfOffset);
        return;
    }

    // 6a. Charge copy gas (dynamic portion).
    let gas_params = context.host.gas_params();
    if !charge_cost_or_oog(context.interpreter, gas_params.copy_cost(len)) {
        return;
    }

    // 6b. Zero-length early return (after copy gas, before memory_offset conversion).
    if len == 0 {
        return;
    }

    // 6c. Convert memory_offset to usize (only needed for non-zero length).
    let Some(memory_offset) = u256_to_usize_or_fail(context.interpreter, memory_offset) else {
        return;
    };

    // 6d. Memory resize — PD-exempt path skips expansion gas, but only when the
    //     destination is contiguous with (or within) the current memory. This
    //     prevents a sparse-write DoS where a small PD buffer is copied to a huge
    //     offset, forcing the node to allocate gigabytes of RAM without paying gas.
    //     When memory_offset <= hwm_bytes the free expansion is bounded by `len`.
    let is_pd = is_current_pd_return_marker(context.interpreter.return_data.buffer());
    let current_hwm_bytes = context.interpreter.gas.memory().words_num * 32;

    if is_pd && memory_offset <= current_hwm_bytes {
        if !resize_memory_without_gas(context.interpreter, gas_params, memory_offset, len) {
            return;
        }
    } else if !context
        .interpreter
        .resize_memory(gas_params, memory_offset, len)
    {
        return;
    }

    // 7. Copy return data into memory.
    context.interpreter.memory.set_data(
        memory_offset,
        data_offset,
        len,
        context.interpreter.return_data.buffer(),
    );
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use revm::bytecode::Bytecode;
    use revm::context_interface::DummyHost;
    use revm::interpreter::interpreter::ExtBytecode;
    use revm::interpreter::interpreter_types::{LoopControl as _, MemoryTr as _};
    use revm::interpreter::{
        InputsImpl, InstructionContext, InstructionResult, Interpreter, SharedMemory, num_words,
    };
    use revm::primitives::U256;
    use revm::primitives::hardfork::SpecId;

    use super::irys_returndatacopy;
    use crate::instructions::pd_return_marker::{
        PdReturnMarkerScope, clear_pd_return_marker, set_pd_return_marker,
    };

    type TestInterpreter = revm::interpreter::interpreter::EthInterpreter;

    /// Build an interpreter with `gas_limit`, return data set to `return_data`,
    /// and three values pushed onto the stack for RETURNDATACOPY:
    /// `[memory_offset, data_offset, len]` (pushed in correct pop order).
    fn build_interpreter(
        gas_limit: u64,
        return_data: Bytes,
        memory_offset: usize,
        data_offset: usize,
        len: usize,
    ) -> Interpreter<TestInterpreter> {
        let bytecode = Bytecode::new_raw(Bytes::from_static(&[0x00])); // STOP
        let mut interp = Interpreter::<TestInterpreter>::new(
            SharedMemory::new(),
            ExtBytecode::new(bytecode),
            InputsImpl::default(),
            false,
            SpecId::CANCUN,
            gas_limit,
        );
        // ReturnDataImpl has a pub inner field: ReturnDataImpl(pub Bytes)
        interp.return_data = revm::interpreter::interpreter::ReturnDataImpl(return_data);

        // Stack is popped in order: [memory_offset, offset, len]
        // Push in reverse so the first pop gets memory_offset.
        assert!(interp.stack.push(U256::from(len)));
        assert!(interp.stack.push(U256::from(data_offset)));
        assert!(interp.stack.push(U256::from(memory_offset)));

        interp
    }

    fn call_handler(interp: &mut Interpreter<TestInterpreter>) {
        let mut host = DummyHost::new(SpecId::CANCUN);
        let ctx = InstructionContext {
            interpreter: interp,
            host: &mut host,
        };
        irys_returndatacopy::<TestInterpreter, DummyHost>(ctx);
    }

    /// The copy gas for `len` bytes: `3 * ceil(len / 32)`.
    fn copy_gas(len: usize) -> u64 {
        3 * num_words(len) as u64
    }

    // -----------------------------------------------------------------------
    // Subtask 1b: PD return data skips memory expansion gas
    // -----------------------------------------------------------------------

    #[test]
    fn pd_return_data_skips_memory_expansion_gas() {
        let _scope = PdReturnMarkerScope::new();

        // 5,120,064 bytes ≈ 20 chunks × 256,000 + 64 ABI overhead
        let size = 5_120_064;
        let data = Bytes::from(vec![0xAB_u8; size]);
        set_pd_return_marker(&data);

        let gas_limit = 100_000_000; // large enough for any path
        let mut interp = build_interpreter(gas_limit, data, 0, 0, size);

        call_handler(&mut interp);

        // Should NOT have halted
        assert!(
            !interp
                .bytecode
                .instruction_result()
                .is_some_and(InstructionResult::is_error),
            "PD 5 MB read should succeed, got: {:?}",
            interp.bytecode.instruction_result()
        );

        let words = num_words(size);
        assert_eq!(words, 160_002);

        // Memory bookkeeping updated
        assert_eq!(interp.gas.memory().words_num, words);

        // Gas spent should be ONLY copy gas, no memory expansion
        let expected_copy = copy_gas(size);
        let gas_spent = interp.gas.spent();
        assert_eq!(
            gas_spent, expected_copy,
            "Gas should be only copy gas ({expected_copy}), got {gas_spent}"
        );

        // Verify memory was actually resized and data copied
        assert!(interp.memory.len() >= size);
        let mem = interp.memory.slice(0..32);
        assert!(mem.iter().all(|&b| b == 0xAB), "Data should be copied");
    }

    // -----------------------------------------------------------------------
    // Subtask 1c: Non-PD return data charges full gas
    // -----------------------------------------------------------------------

    #[test]
    fn non_pd_return_data_charges_memory_expansion() {
        let _scope = PdReturnMarkerScope::new();
        // Do NOT set the PD marker

        let size = 5_120_064;
        let data = Bytes::from(vec![0xCD_u8; size]);
        let gas_limit = 100_000_000;
        let mut interp = build_interpreter(gas_limit, data, 0, 0, size);

        call_handler(&mut interp);

        assert!(
            !interp
                .bytecode
                .instruction_result()
                .is_some_and(InstructionResult::is_error),
            "Non-PD with enough gas should succeed"
        );

        let words = num_words(size);
        let expected_copy = copy_gas(size);
        let expected_expansion = super::expected_memory_cost(words);
        let gas_spent = interp.gas.spent();

        // Gas must include both copy gas AND memory expansion
        assert!(
            gas_spent >= expected_copy + expected_expansion - 1,
            "Non-PD gas ({gas_spent}) should include copy ({expected_copy}) + expansion ({expected_expansion})"
        );
    }

    #[test]
    fn non_pd_large_read_oogs_with_limited_gas() {
        let _scope = PdReturnMarkerScope::new();

        let size = 5_120_064;
        let data = Bytes::from(vec![0xEF_u8; size]);
        // Memory expansion for 5 MB is ~50M gas. Give only 1M.
        let gas_limit = 1_000_000;
        let mut interp = build_interpreter(gas_limit, data, 0, 0, size);

        call_handler(&mut interp);

        assert!(
            interp
                .bytecode
                .instruction_result()
                .is_some_and(InstructionResult::is_error),
            "Non-PD 5 MB with 1M gas should OOG"
        );
    }

    // -----------------------------------------------------------------------
    // Subtask 1d: Zero-length short-circuits
    // -----------------------------------------------------------------------

    #[test]
    fn zero_length_returns_early_no_gas() {
        let _scope = PdReturnMarkerScope::new();

        let data = Bytes::from(vec![0xFF_u8; 1024]);
        set_pd_return_marker(&data);

        let gas_limit = 100_000;
        let mut interp = build_interpreter(gas_limit, data, 0, 0, 0);

        call_handler(&mut interp);

        assert!(
            !interp
                .bytecode
                .instruction_result()
                .is_some_and(InstructionResult::is_error)
        );
        // Zero-length copy gas = 3 * ceil(0/32) = 0
        assert_eq!(
            interp.gas.spent(),
            0,
            "Zero-length should cost zero dynamic gas"
        );
        assert_eq!(interp.gas.memory().words_num, 0, "No memory expansion");
    }

    // -----------------------------------------------------------------------
    // Subtask 1e: PD marker cleared between transactions
    // -----------------------------------------------------------------------

    #[test]
    fn marker_cleared_by_scope_charges_expansion() {
        let data = Bytes::from(vec![0xAA_u8; 1024]);
        set_pd_return_marker(&data);

        // Create and drop a scope — clears the marker
        {
            let _scope = PdReturnMarkerScope::new();
        }

        // Now handler should treat this as non-PD
        let gas_limit = 100_000_000;
        let mut interp = build_interpreter(gas_limit, data, 0, 0, 1024);

        call_handler(&mut interp);

        assert!(
            !interp
                .bytecode
                .instruction_result()
                .is_some_and(InstructionResult::is_error)
        );

        let words = num_words(1024);
        let expected_expansion = super::expected_memory_cost(words);
        let gas_spent = interp.gas.spent();

        // Must include memory expansion (not just copy gas)
        assert!(
            gas_spent > copy_gas(1024) + expected_expansion / 2,
            "After scope drop, gas ({gas_spent}) should include memory expansion"
        );
    }

    // -----------------------------------------------------------------------
    // Subtask 1f: Non-zero source offset
    // -----------------------------------------------------------------------

    #[test]
    fn pd_non_zero_source_offset_still_exempt() {
        let _scope = PdReturnMarkerScope::new();

        let size = 100_000;
        let mut raw = vec![0x00_u8; size];
        // Write a pattern at offset 50,000
        raw[50_000..50_032].fill(0xBE);
        let data = Bytes::from(raw);
        set_pd_return_marker(&data);

        let read_offset = 50_000;
        let read_len = 32;
        let gas_limit = 100_000;
        let mut interp = build_interpreter(gas_limit, data, 0, read_offset, read_len);

        call_handler(&mut interp);

        assert!(
            !interp
                .bytecode
                .instruction_result()
                .is_some_and(InstructionResult::is_error)
        );

        // Only copy gas, no expansion (PD exempt)
        assert_eq!(interp.gas.spent(), copy_gas(read_len));

        // Verify correct bytes were copied
        let mem = interp.memory.slice(0..32);
        assert!(mem.iter().all(|&b| b == 0xBE));
    }

    // -----------------------------------------------------------------------
    // Subtask 1g: Multiple RETURNDATACOPY ops against same PD buffer
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_returndatacopy_both_exempt() {
        let _scope = PdReturnMarkerScope::new();

        let size = 2_000_000; // 2 MB
        let data = Bytes::from(vec![0x42_u8; size]);
        set_pd_return_marker(&data);

        let gas_limit = 100_000_000;

        // First copy: 1 MB at dest_offset=0
        let mut interp = build_interpreter(gas_limit, data, 0, 0, 1_000_000);
        call_handler(&mut interp);
        assert!(
            !interp
                .bytecode
                .instruction_result()
                .is_some_and(InstructionResult::is_error)
        );
        let _gas_after_first = interp.gas.spent();
        let words_after_first = interp.gas.memory().words_num;

        // Second copy: next 1 MB at dest_offset=1_000_000
        // Re-push stack for second call
        assert!(interp.stack.push(U256::from(1_000_000_usize))); // len
        assert!(interp.stack.push(U256::from(1_000_000_usize))); // data_offset
        assert!(interp.stack.push(U256::from(1_000_000_usize))); // memory_offset
        // Reset bytecode action so the handler can execute again
        interp.bytecode.reset_action();

        call_handler(&mut interp);
        assert!(
            !interp
                .bytecode
                .instruction_result()
                .is_some_and(InstructionResult::is_error)
        );

        let gas_after_second = interp.gas.spent();
        let words_after_second = interp.gas.memory().words_num;

        // Both copies should only charge copy gas (no expansion)
        let expected_total = copy_gas(1_000_000) + copy_gas(1_000_000);
        assert_eq!(
            gas_after_second, expected_total,
            "Both copies should charge only copy gas ({expected_total}), got {gas_after_second}"
        );

        // High-water mark should be at 2 MB
        assert_eq!(words_after_second, num_words(2_000_000));
        assert!(words_after_second > words_after_first);
    }

    // -----------------------------------------------------------------------
    // Subtask 1h: High-water mark bookkeeping is exact
    // -----------------------------------------------------------------------

    #[test]
    fn pd_highwater_mark_expansion_cost_is_correct() {
        let _scope = PdReturnMarkerScope::new();

        let size = 5_120_064;
        let data = Bytes::from(vec![0x11_u8; size]);
        set_pd_return_marker(&data);

        let gas_limit = 100_000_000;
        let mut interp = build_interpreter(gas_limit, data, 0, 0, size);

        call_handler(&mut interp);

        assert!(
            !interp
                .bytecode
                .instruction_result()
                .is_some_and(InstructionResult::is_error)
        );

        let words = num_words(size);
        // expansion_cost should match what a normal expansion would compute
        let expected_expansion_cost = super::expected_memory_cost(words);
        assert_eq!(
            interp.gas.memory().expansion_cost,
            expected_expansion_cost,
            "expansion_cost should be updated even though gas was not charged"
        );
        assert_eq!(interp.gas.memory().words_num, words);
    }

    // -----------------------------------------------------------------------
    // Subtask 6b: Failed PD call clears prior exemption
    // -----------------------------------------------------------------------

    /// Simulates the sequence: successful PD call → failed PD call → RETURNDATACOPY.
    ///
    /// The PD precompile clears the marker on entry and only sets it on success.
    /// After a failure, the marker is cleared, so RETURNDATACOPY on the
    /// (non-PD) return data must charge full memory expansion gas.
    #[test]
    fn failed_pd_call_clears_prior_exemption() {
        let _scope = PdReturnMarkerScope::new();

        // Simulate a prior successful PD call by setting the marker.
        let prior_pd_data = Bytes::from(vec![0xAA_u8; 1024]);
        set_pd_return_marker(&prior_pd_data);

        // Simulate a second PD precompile entry: clear_pd_return_marker() is
        // called on every precompile invocation before any work is done.
        clear_pd_return_marker();

        // Simulate PD failure: set_pd_return_marker() is NOT called.
        // The return data is a different allocation (e.g. revert/error bytes).
        let size = 5_120_064;
        let revert_data = Bytes::from(vec![0xEE_u8; size]);

        let gas_limit = 100_000_000;
        let mut interp = build_interpreter(gas_limit, revert_data, 0, 0, size);

        call_handler(&mut interp);

        assert!(
            !interp
                .bytecode
                .instruction_result()
                .is_some_and(InstructionResult::is_error),
            "Should succeed with enough gas"
        );

        let words = num_words(size);
        let expected_copy = copy_gas(size);
        let expected_expansion = super::expected_memory_cost(words);
        let gas_spent = interp.gas.spent();

        // Gas must include memory expansion — the marker was cleared by the
        // failed precompile call, so no exemption applies.
        assert!(
            gas_spent >= expected_copy + expected_expansion - 1,
            "After failed PD call, gas ({gas_spent}) should include copy ({expected_copy}) + expansion ({expected_expansion})"
        );
    }

    // -----------------------------------------------------------------------
    // Regression: sparse PD destination charges expansion gas
    // -----------------------------------------------------------------------

    /// Copying a small PD return buffer to a large sparse memory offset must
    /// charge expansion gas. Without this guard a malicious contract could
    /// allocate gigabytes of memory for free by RETURNDATACOPY-ing a tiny PD
    /// buffer to offset 2^30+.
    #[test]
    fn pd_sparse_destination_charges_expansion_gas() {
        let _scope = PdReturnMarkerScope::new();

        let size = 32;
        let data = Bytes::from(vec![0xAA_u8; size]);
        set_pd_return_marker(&data);

        // Copy 32 bytes of PD data to a destination 1 MB into memory.
        // Current hwm is 0, so memory_offset (1_000_000) > hwm (0).
        // The exemption should NOT apply — expansion gas must be charged.
        let dest_offset = 1_000_000;
        let gas_limit = 10_000_000;
        let mut interp = build_interpreter(gas_limit, data, dest_offset, 0, size);

        call_handler(&mut interp);

        assert!(
            !interp
                .bytecode
                .instruction_result()
                .is_some_and(InstructionResult::is_error),
            "Should succeed with enough gas"
        );

        let words = num_words(dest_offset + size);
        let expected_copy = copy_gas(size);
        let expected_expansion = super::expected_memory_cost(words);
        let gas_spent = interp.gas.spent();

        // Gas must include memory expansion — sparse PD destination is not exempt.
        assert!(
            gas_spent >= expected_copy + expected_expansion - 1,
            "Sparse PD dest: gas ({gas_spent}) should include copy ({expected_copy}) + expansion ({expected_expansion})"
        );
    }
}
