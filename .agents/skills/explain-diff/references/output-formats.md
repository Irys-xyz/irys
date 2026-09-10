# Output formats

Section-by-section writing guidance for each mode, plus the mandatory HTML output spec. Delivery is always HTML — never Notion format.

Universal rules, all modes:

- **Order by importance, not file order.** The reader should be able to stop at any point and have read the most valuable material so far.
- **Cite locations as `file:line`** so they're clickable in the terminal. In the HTML the `file:line` is the visible link text itself, never a hover tooltip (see § HTML output, Citations).
- **Quote small snippets** (a few lines) when the code itself is the clearest explanation; never paste whole hunks.
- **Empty sections say "None."** Do not invent content to fill a skeleton.
- **Mark inference.** "The PR description says X" vs "this appears to be for X" are different claims; keep them distinguishable.

---

## summary mode

Audience: a maintainer triaging their review queue, or an AI agent loading context before working in the area. Budget: roughly one screen (~40 lines) for a typical PR; two sentences for a trivial change.

The skeleton:

```
## <One-line title for the change>

**What it does:** 2–3 sentences.
**Why:** stated intent, or inferred intent clearly marked as inferred.

### Change map
- **<Theme>** (`files`) — one line: what changed and why.

### Behavior changes
- <before> → <after>, for anything user-, API-, or data-visible. "None" is a valid answer.

### Watch out for
- `file:line` — one sentence: what could break and under what conditions. (max 5)
```

Section by section:

- **Title** — name the change by its effect, not its mechanics: "Cache keys become tenant-scoped," not "Update cache.py."
- **What it does** — 2–3 sentences covering the observable outcome.
- **Why** — one line. Stated intent if available, inferred intent marked as inferred.
- **Change map** — one bullet per theme (typically 2–6 themes), boldface theme name, the files in backticks, then one line of what-and-why. A reader should be able to navigate the diff from this map alone.
- **Behavior changes** — only user-, API-, or data-visible differences, as `before → after` in prose. This is the section an agent needs most; be precise about conditions ("only when `retries > 0`").
- **Watch out for** — at most 5, ranked, each one line: `file:line` — what could break, under what conditions. Draw from the analysis lenses. If a callout is serious, append "(worth a dedicated code-review pass)".

## teach-me mode

Audience: a developer who can read code but is new to this codebase, this domain, or professional review. Voice: Martin Kleppmann — concrete before abstract, one idea per paragraph, smooth transitions, zero condescension. Length: as long as it needs to be, but every section must earn its length.

- **Background** — two layers, explicitly labeled. First the broad layer: what this subsystem is for and how data flows through it, written for someone who has never opened these files, and marked "skip if you know the codebase." Then the narrow layer: the specific functions, tables, or flows this change touches. Use a running example with concrete toy data ("a user `alice` with two sessions…") that you'll reuse throughout.
- **The core idea** — the intuition of the change in isolation from the code. State the problem first, then the insight. Walk the running example through the old behavior, show where it goes wrong or falls short, then walk it through the new behavior. Diagrams help here (see HTML spec; in markdown, use nested lists or small tables rather than ASCII art).
- **Guided tour** — theme by theme, in the order that builds understanding (foundations first, even if they're the smaller hunks). Quote a few lines of real code per stop. Define every piece of jargon at first use — in one clause, in place, not in a glossary the reader has to jump to. Connect each theme back to the running example.
- **What could go wrong** — the risk callouts from the lenses, but taught: name the general pattern ("when code deletes a validation check, someone has to prove it's re-established elsewhere — this is called a removed-behavior audit"), show how it applies here, and say how a reviewer would check it. This section is where the reader learns to review, not just to read.
- **Quiz** — 5 multiple-choice questions, medium difficulty: answerable only by someone who understood the substance, but no gotchas or trivia. Each question has 4 options; wrong options should be plausible misreadings, not jokes. In markdown, hide each answer in a `<details><summary>Answer</summary>…</details>` block, and in the answer explain why the right option is right *and* why the tempting wrong option is wrong.

## expert mode

Audience: a domain expert about to review this PR. Zero background, no term definitions, no restating what the reader can see in one glance at the diff. Every sentence load-bearing. Dense is good; cryptic is not — full sentences, precise claims.

- **Intent & approach** — what the change accomplishes and the strategy chosen. If the diff makes an implicit design choice visible (e.g. invalidation chosen over versioned keys), name the road not taken in one line — reviewers disagree with approaches more often than with lines.
- **Delta semantics** — the contract-level truth of the change: invariants added, removed, weakened; state-machine and ordering changes; error-contract changes; compatibility with old data and in-flight requests. Include the **removed-behavior audit table**: each deleted invariant → where it was enforced → where it's re-established now, or **"unaccounted for"** in bold. This table is often the highest-value artifact in the whole output.
- **Cross-file impact** — callers now facing new preconditions, changed return shapes, or new failure modes, as `file:line` — one-line consequence. Only entries where something actually changed for the caller.
- **Review-focus map** — the payload. Ranked list (most severe × most likely first) of where bugs, regressions, or design problems are most likely, each entry: `file:line` — the concern — the **concrete failure scenario** ("state/inputs → wrong outcome"). These are attention pointers, not verified findings; do not present them as confirmed bugs. Cap at what's real — 3 strong entries beat 10 padded ones. Close the section by offering a dedicated code-review pass to verify the serious ones.
- **Open questions** — questions only the author can answer: intent ambiguities, migration/rollout ordering, "was dropping X deliberate?" Questions a reviewer would otherwise have to ask in comments.

---

## HTML output (mandatory — always)

**Always** write one single self-contained HTML file — CSS and JavaScript inline, no external resources. This is the **primary** delivery format for every mode, not an optional add-on.

**Never Notion.** Do not produce Notion pages, Notion markdown exports, Notion block trees, Notion API JSON, or Notion-flavored callouts/toggles/databases. If a tool default would push toward Notion, override it and write HTML instead.

Terminal chat: short status only (mode, comparison range, HTML path). Do not paste the full write-up as chat markdown or Notion content unless the user later asks for a text excerpt.

**File location:** `/tmp/YYYY-MM-DD-explanation-<slug>.html` using today's real date and a short kebab-case slug of the change title. The date prefix keeps files time-sorted and out of version control. Report the full path when done.

**Structure:** one long page (no tabs at the top level) with a sticky or top-anchored table of contents linking to section headers, and the sections of whichever mode is active. Basic responsive styling so it reads well on a phone.

**Sections are not optional.** Render every section of the active mode's skeleton, in skeleton order, each under its own header and linked from the contents. A section with nothing to report keeps its header and says "None." A dropped section is indistinguishable from a forgotten one, so the reader cannot trust the page. In summary mode this most often bites "Watch out for": a rollout condition the change's own docs mention, or an invariant that only convention (a shared template variable, a copied constant) enforces, is a callout, not "None."

**Citations:** every source location is an `<a>` whose visible text is `path:line` (path relative to the repo root, which the page header states once) and whose `href` is the absolute file path, so the link opens the file locally. Do not bury the line number in a `title` attribute or behind a prose label such as "relay template"; the reader must see the line without hovering.

**Citations in CI mode:** when the invoking prompt supplies a citation base URL (`https://github.com/<owner>/<repo>/blob/<sha>`), the `href` is `<base>/<path>#L<line>` rather than an absolute local path, so the link resolves for anyone reading the page from the PR. The visible text is still `path:line`. Cite the line numbers of the tree you were given (the PR head or merge commit), never the diff's `@@` hunk offsets.

**Diagrams:** pick a small family of 1–2 diagram types and reuse them throughout so the reader learns the visual vocabulary once. Useful families:

- a very simplified rendering of the UI the user sees, for UI changes
- a system/data-flow diagram showing components and the data moving between them — always with concrete example data on the arrows, never bare boxes

Build diagrams from simple styled HTML (flexbox boxes, borders, arrows via characters or borders). **Never ASCII art.** Use HTML lists for lists.

**Code blocks:** use `<pre>` tags. If a custom-styled `div` is used instead, it **must** have `white-space: pre` or `pre-wrap` in its CSS or the browser collapses all newlines. Before saving the file, scan every code block in the HTML source and confirm this property is present.

**Callouts:** styled callout boxes for key concepts and definitions, important edge cases, and risk warnings — visually distinct per kind (e.g. info vs warning).

**Quiz (teach-me mode):** the 5 questions become interactive multiple-choice — clicking an option immediately shows whether it was correct plus the explanation for that specific option. No frameworks; a few lines of vanilla JS. Summary and expert mode HTML omit the quiz; expert mode renders the removed-behavior audit and review-focus map as real HTML tables.
