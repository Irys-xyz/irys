# Origin

Upstream: https://github.com/Chris-Graffagnino/explain-diff (MIT; see `LICENSE`).

This directory vendors the upstream skill so that `.github/workflows/pr-explain.yml` can run it against every pull request from a copy the repository controls, and so that contributors can invoke it locally (`/explain-diff [mode] [target]`) with the same instructions CI uses.

## Local modifications

The upstream prose is kept verbatim, including its American spelling, so that future upstream diffs apply cleanly. Two additions carry the CI behaviour:

- `SKILL.md` § CI mode: inputs arrive as pre-fetched files under `.pr-explain/`, the output path is dictated by the prompt, and citations link to GitHub at a fixed commit.
- `references/output-formats.md` § Citations in CI mode: the `href` form for those GitHub links.

`agents/openai.yaml` from upstream (Codex display metadata) is not vendored; nothing here depends on it.
