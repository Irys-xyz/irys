# Analysis lenses

Run these during Step 3 (build understanding), before writing anything. Each lens produces raw material: the change map comes from lens 1, the risk callouts and review-focus map come from lenses 2–7. Not every lens fires on every diff — a docs-only change needs none of them — but *deciding* a lens doesn't apply should take a moment of actual looking, not a reflex.

These lenses direct attention; they do not produce verified findings. Anything serious they surface becomes a "watch out for" / review-focus entry with a concrete failure scenario, plus a suggestion to run a dedicated code-review pass if verification matters.

## 1. Change taxonomy

Classify every hunk as one of:

- **behavior change** — output, side effects, or timing differ for some input
- **new capability** — code paths that didn't exist before
- **removal** — behavior or capability deleted
- **refactor** — claims to preserve behavior while restructuring
- **mechanical** — rename, formatting, import shuffling, generated files
- **config / schema / dependency** — changes to the environment the code runs in
- **test** — test-only changes

The highest-value catch of this lens: a hunk *presented* as mechanical or refactor that actually changes behavior. Diff-readers skim "rename" commits; that is exactly where regressions hide. When a refactor hunk changes behavior, flag it prominently in every mode.

## 2. Removed-behavior audit

For every line the diff **deletes or replaces**, name the invariant, check, or behavior it enforced — then search the new code for where that invariant is re-established. Deleted `if` guards, dropped `await`s, removed error handling, deleted test assertions, and loosened validation all count.

Three outcomes per deleted invariant:

- **re-established** — the new code enforces it elsewhere; cite where.
- **intentionally dropped** — the stated intent covers the removal; say so.
- **unaccounted for** — nobody re-established it and nothing says the drop was intended. This is your strongest class of risk callout; regressions are usually here.

In expert mode this lens produces its own table (invariant → old enforcement site → new enforcement site or "none").

## 3. Cross-file impact tracer

For each changed symbol (function signature, return shape, exception behavior, exported constant, schema):

- **Grep for callers.** Do any now violate a new precondition, mishandle a changed return shape, or fail to catch a new exception?
- **Check callees.** Does the changed code now call something with arguments or in an order its contract doesn't support?
- **Check timing and ordering.** Did anything move across an async boundary, out of a lock, or out of a transaction?

The blast radius of a change is usually in the files the diff *doesn't* touch. Callers that needed updating and didn't get it are prime review-focus entries.

## 4. Invariant and contract delta

Zoom out from lines to contracts. What is **stronger, weaker, or new** after this change in:

- API signatures and response shapes (including error responses)
- data formats: serialization, DB schema, message/queue payloads, file formats
- state machines: new states, removed transitions, changed ordering guarantees
- concurrency assumptions: what may now run in parallel, what lock protects what
- failure behavior: what errors are thrown vs swallowed vs retried, and who is expected to handle them

Weakened contracts and widened types are regression seeds even when every existing test passes. Compatibility questions live here too: can old data / old clients / in-flight requests survive this change?

## 5. Design-pressure check

Is the change implemented at the right altitude, or is it a bandaid?

- **Special cases layered on shared infrastructure** — an `if (customerX)` in generic code is a sign the fix isn't deep enough.
- **Duplication** — new code re-implementing a helper the codebase already has (Grep the utility modules), or copy-paste with slight variation inside the diff itself.
- **New coupling** — a module now importing from a layer it previously didn't know about; leaky abstractions; a dependency arrow pointing the wrong way.
- **Derivable state** — new fields or caches that duplicate information already computable, creating a consistency burden.

These become design-problem callouts. Frame them as pressure, not verdicts: "this adds the third special case to `dispatch()` — the pattern suggests a strategy hook" reads better and is more useful than "bad design."

## 6. Test delta

Compare what changed in the code against what changed in the tests:

- **Behavior changed, tests untouched** — either the tests never covered it (gap worth naming) or they should have failed (worth asking why they didn't).
- **Tests modified to pass** — assertions weakened, fixtures changed, tests deleted or skipped. Sometimes legitimate; always worth one sentence of scrutiny.
- **New behavior, new tests** — check the tests exercise the interesting edges (boundaries, error paths), not just the happy path.

## 7. Hidden-risk heuristics

A quick scan for classic footguns *introduced by the diff* — keep this light; a dedicated code-review pass owns deep bug-hunting:

off-by-one at loop/slice boundaries · falsy-zero or empty-string treated as missing · missing `await` / unhandled promise · error swallowed in a broad catch · wrong variable after copy-paste · mutable default arguments (Python) · closure capturing a loop variable · `==` coercion (JS) · unanchored or unescaped regex · timezone/DST assumptions · float equality · resource acquired but not released on the error path.

Anything that fires becomes a watch-out with the concrete triggering condition, not just the pattern name.

## Feeding the lenses into each mode

- **summary** — lens 1 builds the change map; the strongest items from lenses 2–7 become "Watch out for," ranked by severity × likelihood (cap defined in `output-formats.md`).
- **teach-me** — the same material, but each callout is *explained*: what the risk pattern is in general, why it applies here, and how a reviewer would check it. The goal is teaching review thinking, not just listing risks.
- **expert** — everything survives: the removed-behavior table (lens 2), cross-file impact (lens 3), contract delta (lens 4) in "Delta semantics," design pressure (lens 5) and the rest ranked in the review-focus map, each entry carrying a concrete failure scenario ("inputs/state → wrong outcome").
