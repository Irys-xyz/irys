# Build Versioning

## Status

Accepted

## Context

Nodes in a decentralized network need to identify themselves to peers — both the protocol-level version they speak and the specific software producing it. Without this, operators cannot diagnose version mismatches, coordinate upgrades, or distinguish between alternative implementations of the same protocol.

The version reported by a node should:
1. Follow a widely understood format (SemVer).
2. Identify the implementation (vendor), since multiple implementations of the protocol may coexist.
3. Distinguish tagged releases from development builds, so operators can tell "release 3.0.0" from "some commit on main after 3.0.0".
4. Be derived automatically from the build environment — never manually maintained.

## Decision

### Version format

Node versions follow [SemVer 2.0.0](https://semver.org/), using build metadata to carry implementation and provenance information:

| Scenario | Format | Example |
|---|---|---|
| Tagged release | `MAJOR.MINOR.PATCH+VENDOR` | `3.0.0+irys-rs` |
| Tagged, dirty | `MAJOR.MINOR.PATCH+VENDOR.dirty` | `3.0.0+irys-rs.dirty` |
| Development build | `MAJOR.MINOR.PATCH+VENDOR.GIT_SHA` | `3.0.0+irys-rs.a1b2c3d` |
| Development, dirty | `MAJOR.MINOR.PATCH+VENDOR.GIT_SHA.dirty` | `3.0.0+irys-rs.a1b2c3d.dirty` |

**VENDOR** identifies the implementation (`irys-rs` for this codebase). Alternative implementations would use their own identifier (e.g. `irys-go`). **GIT_SHA** is the 7-character short commit hash, appended only for builds not on an exact release tag. **dirty** is appended when the working tree contains uncommitted changes (staged or unstaged, excluding untracked files) at build time.

Build metadata is ignored for SemVer precedence — `3.0.0+irys-rs.abc` and `3.0.0+irys-rs.def` are equal when compared for compatibility.

Note: "tagged" means any exact tag on HEAD (`git describe --exact-match --tags`), not only tags matching a release naming convention.

### Workspace version

Each crate in the workspace defines its own version independently (e.g. `irys-chain = "3.0.0"`, `irys-types = "0.1.0"`). The node's reported version comes from the binary crate's `Cargo.toml` — `init_version` in `main()` receives it via `env!("CARGO_PKG_VERSION")` at the call site.

### Build-time capture

The main binary's build script captures the git SHA (pinned to 7 characters via `--short=7`), tag status, and working-tree cleanliness at compile time. Dirty detection uses `git diff-index --quiet HEAD --`, which covers staged and unstaged changes but intentionally ignores untracked files. The build **fails** if git is unavailable — release artifacts must always carry provenance. Cargo `rerun-if-changed` directives ensure the version updates when commits or tags change.

### Runtime model

A global, initialise-once cell holds the version. Binary crates populate it with git metadata as the first action in `main()`. Library crates and tests access it through a function that falls back to the plain workspace version with vendor metadata if no binary initialiser ran.

### Role in P2P

The version is **strictly informational** — it is carried in handshake messages for observability only. It **must not** be used for gossip decisions, peer acceptance/rejection, or compatibility checks. Peer compatibility is determined solely by the protocol version.

**V1 handshakes** sign only `major.minor.patch` — pre-release and build metadata are excluded. This cannot be changed because V1 is already live on mainnet; altering the preimage would break signature verification with existing peers. Build metadata in V1 is therefore unauthenticated and could be modified in transit.

**V2 handshakes** sign the full semver string (including pre-release and build metadata). This makes the version an authenticated part of the handshake — a peer cannot misrepresent its build without invalidating the signature. V2 is not yet live on mainnet, so this stronger invariant can be established from the start.

### Wire format

The version is serialized as a plain SemVer string (e.g. `"3.0.0+irys-rs.a1b2c3d"`). Deserialization accepts any valid SemVer 2.0.0 string, preserving unknown metadata from other implementations.

## Consequences

- Operators can identify exact builds in logs and peer handshakes.
- The vendor tag prepares for a multi-implementation ecosystem.
- Development builds are distinguishable from releases at a glance.
- Dirty builds are clearly marked, aiding debugging and discouraging release artifacts built from uncommitted changes.
- Builds require a git repository — source tarballs without `.git` will not compile. This is an intentional trade-off favouring provenance over convenience.
- The initialise-once model has an ordering constraint: the binary must populate the version before any code reads it. This is documented at both the API and call-site level.
