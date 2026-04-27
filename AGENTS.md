# AGENTIC DIRECTIVE

> Identical to CLAUDE.md (which just points back here). Architecture rules
> live in [PLAN.md](PLAN.md); this file holds the operating contract.

## Toolchain

- Rust workspace + Tauri 2 desktop. **No Python.**
- Stable Rust, Node.js ≥ 20, npm. Frontend is vanilla TS + Vite.
- Dev: `npm install && npm run tauri dev`.
- Release: `npm run tauri build` (NSIS on Windows).
- CI gate (in order): `cargo fmt --all`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace`.

## Engineering principles

- **Zero-defect**: fix root causes, not symptoms.
- **Minimal**: write the simplest code that works; remove dead code in the
  same change rather than leaving shims.
- **Crate boundaries**: respect the dependency direction in
  [PLAN.md](PLAN.md). Don't redefine protocol types outside `proxy-core`;
  don't import upward (e.g. `nim-client` cannot see `proxy-server`).
- **Encapsulation**: use accessor methods, not `_attribute` reaches from
  outside the owning module.
- **Errors**: surface with `anyhow::Result` or `thiserror` enums. No
  `unwrap` in fallible paths.
- **Performance**: cache config at startup, prefer `format!` over `+=`,
  reach for `parking_lot` only where contention matters.
- **Release size**: don't regress the size-tuned `[profile.release]` (see
  PLAN.md). Add deps with `default-features = false` where reasonable.

## Workflow

1. **Analyze** — read the relevant files; do not guess.
2. **Plan** — order changes by dependency.
3. **Execute** — incrementally, with focused commits.
4. **Verify** — run fmt + clippy + tests; confirm via logs.
5. **Specificity** — do exactly what's asked, no more, no less.

## Summaries

Technical and granular. Always include:
[Files Changed], [Logic Altered], [Verification Method],
[Residual Risks] (say "none" if there are none).
