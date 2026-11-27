# Changelog

## 1.1.0 — 2026-11-27

- First publication to the MCP Registry as `ai.qompass/periscope` (Registry onboarding scoped by the release sweep record, ~/workspace/release-sweep-20261127/RECORD.md, held item 7). server.json package surface is now the cargo entry (`periscope-mcp` 1.1.0, stdio): the MCPB bundle entry's fileSha256 still named the 0.1.0 bundle (noted in the 1.0.0 entry below), so it is retired rather than carried forward stale.
- README gains the `mcp-name: ai.qompass/periscope` ownership marker the Registry verifies against the crates.io package.
- No tool-surface or behaviour change from 1.0.0.

## 1.0.0 — 2026-10-31

- rmcp modern-only migration landed (rmcp 3.8.0, protocol 2026-07-28; WS2-W2 gate pass).
- Toolchain pin swept to nightly-2026-10-30 (rustc 1.101.0-nightly 92b5b8c87, clippy 0.1.101).
- Gates on the new pin: cargo test 34 passed / 0 failed; clippy --all-targets -D warnings clean; fmt --check clean; no future-incompatibilities reported.
- server.json advanced to 1.0.0 (bundle identifier now points at the v1.0.0 release path; fileSha256 still names the 0.1.0 bundle — MCPB repackage + MCP Registry republish are the pending primo-side leg, not claimed here).
