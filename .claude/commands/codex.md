# Codex Second Opinion

Get a second opinion from OpenAI Codex on any question about this codebase — debugging, analysis, architecture, code review, or anything else.

## Arguments

The user's request is: $ARGUMENTS

## Instructions

Your job is to gather the right context, build a prompt that gives Codex enough understanding of this codebase to be useful, and then run `codex exec` with that prompt. Codex has zero knowledge of this codebase, so the context you provide is everything.

### Step 1: Gather Context

Read the Architecture Overview section of `CLAUDE.md` for a high-level understanding. Then, based on what the user is asking about, selectively read the files that are most relevant. Use these heuristics:

- **If actor services are involved**: read the service's message types and the `ServiceSenders`/`ServiceReceivers` pattern
- **If types crate is involved**: check wire formats (`BlockBody`, `BlockHeader`, `DataTransactionHeader`)
- **If consensus/mining is involved**: read the VDF + PoA section and shadow transaction patterns
- **If p2p is involved**: check gossip protocol routes and circuit breaker usage
- **If storage/packing is involved**: check chunk size constants and XOR packing invariants
- **If reth integration is involved**: check CL/EL boundary and payload building flow

If the user's request references specific files, diffs, or branches, read those too. Keep context focused — aim for 3-5 key files maximum.

### Step 2: Build the Prompt

Construct a single prompt string that includes:

1. **Architecture summary** — 2-3 sentences describing the relevant components and how they fit together
2. **Key conventions** — patterns that apply (e.g., "custom Tokio channel-based actor system, not Actix", "crypto crates compiled with opt-level=3")
3. **Relevant code** — inline the key snippets or file contents that Codex needs to see
4. **The user's request** — what they actually want Codex to analyze

### Step 3: Run Codex

```bash
codex exec --sandbox read-only "<constructed prompt>"
```

Run this in the background with a 300s timeout.

### Step 4: Monitor Progress

After launching codex in the background:

1. Wait ~30 seconds, then check the background task output using `TaskOutput` with `block: false`.
2. If there is new output, give the user a brief progress update.
3. Repeat every ~30 seconds.
4. If no new output appears for 60+ seconds and the task hasn't completed, warn the user that codex may be stuck and offer to kill it.

### Step 5: Present Results

Once codex finishes:

1. **Summary**: Concise summary of key findings, organized by category (only include categories with findings):
   - Bugs and logic errors
   - Security concerns
   - Concurrency / actor system issues
   - Code quality and style
   - Performance considerations

2. **Raw output**: Complete codex output in a fenced code block.

3. **Counterpoints**: If you (Claude) disagree with any findings or think something was missed, add a "Claude's take" section. Only include this if you have a meaningful counterpoint.

### Error Handling

- If `codex` is not found, tell the user to install it: `npm install -g @openai/codex`
- If codex times out (5 minutes), show whatever partial output was captured and note the timeout
