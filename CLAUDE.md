# Irys Chain — Claude Code Instructions

## Before pushing or creating PRs

Always run these checks and fix any issues before running `git push` or `gh pr create`:

1. `cargo fmt --all`
2. `cargo clippy --workspace --tests --all-targets`
