# Tracing Guidelines

## 1. Instrument All Logging Functions

**If a function contains a tracing macro (`info!`, `debug!`, `warn!`, `error!`, `trace!`), it should be instrumented with `#[instrument]`.**

```rust
// ❌ BAD - Using tracing without instrumentation
async fn process_transaction(tx: Transaction) -> Result<()> {
    info!(tx.id = %tx.id(), "Processing transaction");
    Ok(())
}

// ✅ GOOD - Instrumented function
#[instrument(skip_all, fields(tx.id = %tx.id()))]
async fn process_transaction(tx: Transaction) -> Result<()> {
    info!("Processing transaction");
    Ok(())
}
```

> NOTE:  If the function doesn't seem like it should be instrumented then we should analyse the logging in the function/method to see if it is required. 

## 2. Use `level = "trace"` for Spans

Instrument spans should be at `trace` level to keep them below the noise of regular logs:

```rust
// ❌ BAD - Span at INFO level (default)
#[instrument(skip_all)]
async fn validate_block(block: Block) -> Result<()> {
    debug!("Validating block");
}

// ✅ GOOD - Span at TRACE level
#[instrument(level = "trace", skip_all)]
async fn validate_block(block: Block) -> Result<()> {
    debug!("Validating block");  // Logs can still be info, debug, etc.
    info!("Block validated");    // Use appropriate level for each log
}
```

This way:
- Running with `RUST_LOG=info` shows only `info!` logs, no spans
- Running with `RUST_LOG=debug` shows `info!` + `debug!` logs, no spans
- Running with `RUST_LOG=trace` shows everything including spans

**Exception:** Use `level = "info"` for high-level operations where you want the span always visible (e.g., request handlers, major state transitions).

## 3. Always Use `skip_all`

Default to skipping all parameters to avoid:
- Expensive Debug formatting
- Accidentally logging sensitive data
- Performance overhead

```rust
// ❌ BAD - Will format all parameters
#[instrument(level = "trace")]
async fn validate_block(block: Block, state: ChainState) -> Result<()> {
    // Expensive and potentially leaks data
}

// ✅ GOOD - Skip all, explicitly add safe fields
#[instrument(level = "trace", skip_all, fields(block.height = %block.height, block.hash = %block.hash))]
async fn validate_block(block: Block, state: ChainState) -> Result<()> {
    // Only logs what you explicitly specify
}
```

## 4. Use `err` for Automatic Error Logging when if makes sense

Add `err` to `#[instrument]` when you want automatic error logging:

```rust
// Without err - errors are not automatically logged
#[instrument(level = "trace", skip_all)]
async fn risky_operation() -> Result<()> {
    dangerous_operation()?;  // Error propagated but not logged
    Ok(())
}

// With err - errors automatically logged at ERROR level
#[instrument(level = "trace", skip_all, err)]
async fn risky_operation() -> Result<()> {
    dangerous_operation()?;  // Error logged AND propagated
    Ok(())
}
```

**When to use `err`:**
- Functions where errors are unexpected and need visibility
- Public API boundaries
- Operations where error context is important for debugging

**When to skip `err`:**
- Functions where errors are common/expected (e.g., validation, lookups)
- When you manually log errors with additional context
- Hot paths / performant code where error logging overhead matters

## 5. Use Field Naming Conventions

Use namespaced field names for consistency and observability:

### Standard Prefixes

| Prefix | Usage | Examples |
|--------|-------|----------|
| `block.*` | Block-related data | `block.height`, `block.hash`, `block.timestamp` |
| `tx.*` | Transaction data | `tx.id`, `tx.signer`, `tx.amount`, `tx.anchor` |
| `peer.*` | Network peer info | `peer.id`, `peer.address`, `peer.status` |
| `account.*` | Account state | `account.balance`, `account.address`, `account.nonce` |
| `node.*` | Node metadata | `node.name`, `node.version`, `node.mode` |
| `error.*` | Error details | `error.code`, `error.message`, `error.source` |
| `chunk.*` | Chunk data | `chunk.data_root`, `chunk.offset`, `chunk.size` |
| `mempool.*` | Mempool state | `mempool.size`, `mempool.pending_count` |
| `validation.*` | Validation info | `validation.result`, `validation.duration` |
| `custom.*` | Non-standard fields | `custom.retry_count`, `custom.batch_size` |


## 6. Span Naming

`#[instrument]` automatically uses the function name as the span name. **This is preferred.**

```rust
// ✅ GOOD - Function name is descriptive, span name is automatic
#[instrument(level = "trace", skip_all, err)]
async fn validate_block_merkle_proof(block: Block) -> Result<()> {
    // Span name: "validate_block_merkle_proof"
    debug!("Validating proof");
    Ok(())
}
```

### If the function name isn't descriptive enough, rename the function:

```rust
// ❌ BAD - Generic function name, trying to fix with custom span name
#[instrument(level = "trace", skip_all, name = "handle_block_producer_message")]
async fn handle_message(msg: Message) -> Result<()> {
    // ...
}

// ✅ BETTER - Just rename the function
#[instrument(level = "trace", skip_all)]
async fn handle_block_producer_message(msg: BlockProducerMessage) -> Result<()> {
    // Span name: "handle_block_producer_message"
}
```

### "If You Really Have To"

Only use custom span names when renaming the function is not possible (e.g., trait implementations, generated code):

```rust
// Trait requires specific function name, but we want better observability
impl Handler<GenericMessage> for MyActor {
    #[instrument(level = "trace", skip_all, name = "block_producer_handler")]
    async fn handle(&mut self, msg: GenericMessage) -> Result<()> {
        // Span name: "block_producer_handler" instead of just "handle"
        debug!("Processing block producer message");
        Ok(())
    }
}
```

## 7. Propagate Context Across Async Boundaries

### Spawn

When spawning tasks, propagate the current span:

```rust
use tracing::Instrument;

#[instrument(level = "trace", skip_all, fields(block.height = %height))]
async fn process_block_async(height: u64) -> Result<()> {
    debug!("Starting async processing");

    // ✅ GOOD - Span propagated to spawned task
    tokio::spawn(
        async move {
            debug!("Background processing"); // Logs in parent span
        }
        .instrument(tracing::Span::current())
    );

    Ok(())
}
```

### For Awaits

Use `.in_current_span()` for simpler cases. **Important:** Attach the span BEFORE awaiting:

```rust
#[instrument(level = "trace", skip_all)]
async fn send_message(actor: &Actor, msg: Message) -> Result<()> {
    debug!("Sending message");

    // ✅ CORRECT - Attach span, THEN await
    actor.send(msg)
        .in_current_span()  // First: attach span
        .await?;            // Then: await

    Ok(())
}

// ❌ WRONG - This doesn't work
async fn send_message_wrong(actor: &Actor, msg: Message) -> Result<()> {
    actor.send(msg)
        .await              // Awaits without span context
        .in_current_span(); // Too late - already completed
}
```

### Manually Passing Spans

For explicit span control, pass the span as a function argument:

```rust
use tracing::Span;

// Caller: Pass the current span
#[instrument(level = "trace", skip_all, fields(block.height = %block.height))]
async fn process_block(block: Block) -> Result<()> {
    debug!("Processing block");

    // Pass current span to helper
    validate_with_span(block.clone(), Span::current()).await?;

    Ok(())
}

// Accept and enter the span
async fn validate_with_span(block: Block, span: Span) -> Result<()> {
    let _enter = span.enter();  // Enter the passed span

    debug!("Validating block");  // Logs in parent's span
    // Validation logic...

    Ok(())
}  // Span is exited when _enter is dropped
```

**When to use this pattern:**
- Passing span across service/actor boundaries
- When you need explicit control over span lifecycle
- Helper functions that should run in caller's context
- Can't use `.instrument()` due to code structure

**Tip:** Prefer `.instrument()` or `.in_current_span()` when possible. Manual span passing is more verbose but gives maximum control.

