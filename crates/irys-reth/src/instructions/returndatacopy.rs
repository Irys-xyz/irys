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

    // 6d. Memory resize — PD-exempt path skips expansion gas.
    let is_pd = is_current_pd_return_marker(context.interpreter.return_data.buffer());

    if is_pd {
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
