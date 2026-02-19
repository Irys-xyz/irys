# Codex Second Opinion

Get a second opinion from OpenAI Codex on code reviews, debugging, and analysis.

## Arguments

The user's input is: $ARGUMENTS

## Instructions

Parse the arguments and determine which mode to use, then gather context and execute the appropriate codex command.

### Mode Detection

Analyze `$ARGUMENTS` to determine the mode:

1. **Branch review** — arguments match `review`, `review branch`, `review --base`, or no arguments at all (default mode). Reviews the current branch against `master`.
2. **PR review** — arguments match patterns like `review PR #42`, `review pr 42`, `pr 42`, `pr #42`
3. **Uncommitted changes review** — arguments match `review uncommitted`, `uncommitted`, `wip`
4. **Commit review** — arguments match `review commit <SHA>`, `commit <SHA>`
5. **Free-form** — anything else (e.g. `debug why test_foo fails`, `explain the block validation flow`)

### Context Gathering (All Modes)

Before running codex, you must build a targeted review prompt. This is the most important step — codex has no knowledge of this codebase, so you need to give it the right context.

#### Step 1: Identify touched areas

Get the diff for the relevant mode:

| Mode | Command |
|---|---|
| Branch review | `git diff master...HEAD --stat` then `git diff master...HEAD` |
| PR review | `gh pr diff <N>` |
| Uncommitted | `git diff HEAD --stat` then `git diff HEAD` (includes staged + unstaged) |
| Commit | `git show <SHA> --stat` then `git show <SHA>` |

From the `--stat` output, identify which crates and modules are touched (e.g., `crates/actors/src/block_tree/`, `crates/types/src/`).

#### Step 2: Read relevant context

Based on the touched areas, read the relevant sections of `CLAUDE.md` (specifically the Architecture Overview section). Then selectively read files that provide context for the review:

- **If actor services are touched**: read the service's message types and the `ServiceSenders`/`ServiceReceivers` pattern
- **If types crate is touched**: check for breaking changes to wire formats (`BlockBody`, `BlockHeader`, `DataTransactionHeader`)
- **If consensus/mining is touched**: read the VDF + PoA section and shadow transaction patterns
- **If p2p is touched**: check gossip protocol routes and circuit breaker usage
- **If storage/packing is touched**: check chunk size constants and XOR packing invariants
- **If reth integration is touched**: check CL/EL boundary and payload building flow

Keep the context focused — only read files directly relevant to the diff. Aim for 3-5 key files maximum.

#### Step 3: Build the review prompt

Construct a prompt that includes:

1. **Architecture summary** — a 2-3 sentence description of what the touched components do and how they fit together, derived from your reading
2. **Key conventions** — the specific patterns that apply (e.g., "This codebase uses a custom Tokio channel-based actor system, not Actix" or "Crypto crates must compile with opt-level=3")
3. **Review focus areas** — what to pay attention to based on the diff:
   - Unsafe code and memory safety (especially in packing/crypto crates)
   - Correctness of `Arc`/`Clone` patterns in actor message passing
   - Wire format backward compatibility (types crate changes)
   - Concurrency bugs (deadlocks, race conditions in channel-based services)
   - Error handling (are errors propagated correctly, or silently swallowed?)
   - Off-by-one errors in chunk/partition/offset calculations

### Execution by Mode

#### Mode 1: Branch Review (Default)

Run codex review with the base flag and your constructed prompt:
```bash
codex review --base master "<constructed review prompt>"
```
Run this in the background with a 300s timeout.

#### Mode 2: PR Review

1. Extract the PR number from the arguments.
2. Gather PR metadata:
   ```bash
   gh pr view <N> --json title,body,labels,baseRefName
   ```
3. Get the diff via `gh pr diff <N>`.
4. Include the PR title and description in the constructed prompt.
5. Run codex with the combined context:
   ```bash
   echo "<constructed prompt including PR context and diff>" | codex exec --sandbox read-only -o /tmp/codex-review-$$.txt -
   ```
   Run this in the background with a 300s timeout.

#### Mode 3: Uncommitted Changes Review

```bash
codex review --uncommitted "<constructed review prompt>"
```
Run in background with 300s timeout.

#### Mode 4: Commit Review

Extract the commit SHA from the arguments. Run:
```bash
codex review --commit <SHA> "<constructed review prompt>"
```
Run in background with 300s timeout.

#### Mode 5: Free-form

Pass the arguments as a prompt to codex exec, prefixed with the architecture context you gathered:
```bash
codex exec --sandbox read-only -o /tmp/codex-review-$$.txt "<architecture context>\n\nUser request: $ARGUMENTS"
```
Run in background with 300s timeout.

### Progress Monitoring

After launching codex in the background:

1. Wait ~30 seconds, then check the background task output using `TaskOutput` with `block: false`.
2. If there is new output, give the user a brief progress update (e.g. "Codex is analyzing the diff..." or quote a snippet of what it's working on).
3. Repeat every ~30 seconds.
4. If no new output appears for 60+ seconds and the task hasn't completed, warn the user that codex may be stuck and offer to kill the process.
5. When the task completes, proceed to output presentation.

### Output Presentation

Once codex finishes:

1. **Summary**: Present a concise summary of key findings organized by category:
   - Bugs and logic errors
   - Security concerns
   - Concurrency / actor system issues
   - Code quality and style suggestions
   - Performance considerations
   Only include categories that have findings.

2. **Raw output**: Include the complete codex output in a fenced code block so the user can read the full analysis.

3. **Counterpoints**: If you (Claude) disagree with any of codex's findings or think something was missed, add a "Claude's take" section noting your perspective. This is especially valuable when codex lacks the architectural context to understand why something was done a certain way. Only include this if you have a meaningful counterpoint — don't add it just for the sake of it.

### Error Handling

- If `codex` is not found, tell the user to install it: `npm install -g @openai/codex`
- If `gh` is not found (PR mode only), tell the user to install GitHub CLI
- If codex times out (5 minutes), show whatever partial output was captured and note the timeout
- If the PR number doesn't exist, report the gh error clearly
- If the current branch IS master (branch review mode), tell the user and suggest using `uncommitted` or `commit <SHA>` mode instead
