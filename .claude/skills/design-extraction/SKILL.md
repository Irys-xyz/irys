---
name: design-extraction
description: Extract architectural and design decisions from a GitHub PR into ADR (Architecture Decision Record) files in design/docs/. Use this skill whenever the user wants to extract design decisions from a PR, create ADRs from PR discussions, document architectural choices from pull requests, or mentions "design extraction" or "design decisions" in the context of a PR. Also triggers for "extract ADRs", "document decisions from PR", or "pull design docs from PR".
---

# Design Extraction

Extract architectural and design decisions from a GitHub PR and write them as ADR files in `design/docs/`.

## Command

```text
/design-extraction <pr-number>
```

## Arguments

- `pr-number` (required): The GitHub PR number to extract decisions from.

## What This Does

PRs often contain important architectural decisions buried in descriptions and comment threads. This skill pulls those decisions out and writes them as structured ADR (Architecture Decision Record) files — short documents that capture *what* was decided, *why*, and *what follows from it*. Future contributors can then understand the reasoning behind the system's design without trawling through old PRs.

## Execution Protocol

### 1. Resolve PR Details

Fetch the PR metadata, body, and all comments:

```bash
gh api "repos/{owner}/{repo}/pulls/<pr-number>"
```

Extract:
- PR number, title
- PR body (the description)
- Head ref, base ref

Then fetch all three comment sources:
- **Issue comments**: `gh api "repos/{owner}/{repo}/issues/<pr-number>/comments" --paginate`
- **Review comments** (inline on code): `gh api "repos/{owner}/{repo}/pulls/<pr-number>/comments" --paginate`
- **Review bodies**: `gh api "repos/{owner}/{repo}/pulls/<pr-number>/reviews" --paginate`

Display the PR title and URL for context.

### 2. Extract the Design Plan

Search for a design plan between HTML comment markers in **both** the PR body and all PR comments:

```html
<!-- design plan -->
... design content here ...
<!-- end of design plan -->
```

Check for these markers in this order:
1. **PR body** — check first
2. **Issue comments** — check all comments chronologically
3. **Review comments** (inline on code) — check all
4. **Review bodies** — check all

Extract content from **every** location where markers are found. If markers appear in multiple places, concatenate the extracted sections in the order listed above — later sections may refine or extend earlier ones.

This combined extracted content is the **primary** source of decisions.

If no design plan markers exist anywhere, use the full PR body as the source material — but apply a higher bar for what counts as a "decision" (skip vague descriptions, feature lists without rationale, etc.).

### 3. Assess Whether There Are Decisions to Extract

If the design plan (or PR body) contains no concrete decisions — it's a placeholder like "TBD", "TODO", "see Slack", "WIP", or just a feature description without architectural rationale — stop and tell the user:

```text
No actionable design decisions found in PR #<number>.
```

A "decision" means a deliberate choice between alternatives with stated rationale — not just a description of what was built.

### 4. Read Existing Design Docs

```text
Glob: design/docs/*.md
```

Read every existing file to understand what's already documented. This is essential for deduplication and for deciding whether to create new files or append to existing ones.

If `design/docs/` doesn't exist, create it:

```bash
mkdir -p design/docs
```

### 5. Analyse PR Comments

PR comments may contain amendments, clarifications, or explicit decisions that refine or supersede the design plan. When processing comments:

- **Override the plan** only when a comment contains a clear resolution, correction, or final decision (e.g., "We decided to go with X instead", "After discussion, the approach is Y")
- **Ignore** questions, speculative remarks, and casual discussion

#### CodeRabbit Comment Filtering

Comments from `@coderabbitai` (CodeRabbit) have a specific structure. Only the **issue description** at the top of the comment is useful input — the rest is machine-generated scaffolding that must be stripped. Specifically:

- **Keep**: The initial description of the issue (everything before the first section marker below)
- **Strip entirely**:
  - `🧩 Analysis chain` section and all content under it
  - `🤖 Prompt for AI Agents` section and all content under it
  - Any `🏁 Scripts executed` section

For example, given a CodeRabbit comment like:

```
⚠️ Potential issue | 🟡 Minor

Improve handling of large integers in JSON-to-TOML conversion.

The fallback to as_f64() can lose precision for large integers...

🧩 Analysis chain
<... strip everything from here down ...>

🤖 Prompt for AI Agents
<... strip everything from here down ...>
```

Only feed the text **above** the first `🧩` or `🤖` marker into the decision extraction pipeline. The analysis chain and AI agent prompts are CodeRabbit internals, not human design decisions.

#### Other Bot Comments

Ignore comments from other bots (GitHub Actions, CI bots, etc.) unless they contain design plan markers (`<!-- design plan -->`).

### 6. Deduplicate

Before writing anything, check each extracted decision against existing design docs. Match by **topic and substance** — if the same decision exists with different wording, skip it. This prevents duplication when the skill is run multiple times or across related PRs.

### 7. Determine Actions

For each decision or coherent set of related decisions:

- **New distinct topic** → `create` a new file
- **Extends an existing doc** → `append` to that file
- **Relates to multiple existing docs** → update the most closely related one and add cross-references: `See also: [Related Topic](related-topic.md)`

### 8. Write ADR Files

Each ADR follows this structure:

```markdown
# <Descriptive Title>

## Status
Accepted

## Context
<What problem or requirement prompted this decision>

## Decision
<What was decided and why>

## Consequences
- <Positive, negative, and neutral consequences>

## Source
PR #<number> — <title>
```

**Filename rules:**
- Kebab-case derived from the topic (e.g., `authentication-strategy.md`, `data-pipeline-architecture.md`)
- No dates or sequence numbers
- Only lowercase letters, digits, and hyphens; must start with a letter or digit
- Must match: `^[a-z0-9][a-z0-9-]*\.md$`

**Content limits:**
- 500 lines maximum per file. If a topic needs more, split into multiple files with cross-references.

**For `create` actions:** Write the full ADR to a new file in `design/docs/`.

**For `append` actions:** Add two blank lines then the new ADR section to the end of the existing file.

### 9. Present Summary

After writing, display:

```text
Design Extraction Complete — PR #<number>: <title>

  Created: <list of new files>
  Updated: <list of appended files>
  Skipped: <count> (already documented)

Summary: <one-line description of what was extracted>
```

## ADR Example

For reference, here's what a well-formed ADR looks like:

```markdown
# Tmpfs for Test Isolation

## Status
Accepted

## Context
Test runners share the host filesystem, causing test pollution between concurrent jobs.

## Decision
Mount an 8 GB tmpfs at `/workspace/tmp` for each test runner container. Size is configurable via the `TMPFS_SIZE` environment variable. Regular storage at `/workspace` is preserved for cross-step artifacts.

## Consequences
- Eliminates test pollution between concurrent jobs
- RAM-backed storage improves I/O performance for test artifacts
- Reduces available host memory by the configured tmpfs size per runner

## Source
PR #42 — Add tmpfs for test runners
```

## Important Guidelines

1. **Decisions, not descriptions**: Only extract deliberate architectural choices with rationale. "We use Redis" is not a decision. "We chose Redis over Memcached because we need pub/sub for real-time invalidation" is.
2. **Preserve intent**: Capture the *why* faithfully. Don't paraphrase away the reasoning.
3. **Be concise**: ADRs should be short and scannable. A few paragraphs per section, not essays.
4. **No git operations**: Write files only. The user handles staging and committing.
5. **Idempotent**: Running twice on the same PR should not create duplicates if the docs already exist.
