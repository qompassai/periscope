# Changelog

## 1.0.0 — 2026-10-31

- rmcp modern-only migration landed (rmcp 3.8.0, protocol 2026-07-28; WS2-W2 gate pass).
- Toolchain pin swept to nightly-2026-10-30 (rustc 1.101.0-nightly 92b5b8c87, clippy 0.1.101).
- Gates on the new pin: cargo test 34 passed / 0 failed; clippy --all-targets -D warnings clean; fmt --check clean; no future-incompatibilities reported.
- server.json advanced to 1.0.0 (bundle identifier now points at the v1.0.0 release path; fileSha256 still names the 0.1.0 bundle — MCPB repackage + MCP Registry republish are the pending primo-side leg, not claimed here).
