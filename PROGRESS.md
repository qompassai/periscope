# Periscope — build record

Commissioned 2026-10-11 by Matt (route B, Neovim-brokered, chosen
over the standalone config-mirror design). Slice 1 = broker spine +
MCP stdio front end + 10 read-only tools.

## Gates (all firsthand, 2026-10-11)

- Sandbox + primo: `cargo test` 16/16 (5 broker/fake-editor,
  1 Lua-syntax over every embedded snippet, 10 MCP protocol).
- `cargo clippy --all-targets -- -D warnings`: clean, both machines.
- `cargo fmt --check`: clean, both machines.
- Primo IDE gate: headless Neovim with the live diver config,
  rustana_ls attached to all 8 crate source buffers, diagnostics 0.
- Toolchain pinned 1.99.0 (`rust-toolchain.toml`); `#![forbid(unsafe_code)]`.

## Live smoke (primo, managed mode, cold sessions)

- Rust scratch project: first-call hover returned rust-analyzer's
  real payload after the progress-gated wait; definition resolved
  `add` to main.rs:0:3; document symbols correct.
- Diagnostics on a file with a deliberate type error returned empty
  *faithfully*: `lsp/rustana_ls.lua` sets
  `rust-analyzer.diagnostics.enable = false` (bacon-ls owns Rust
  diagnostics in this config, and only while bacon runs).
  `attached_servers: ["rustana_ls"]` tells the caller coverage is
  partial. Not a broker bug.
- Diver repo workspace: 359 configured servers, 136 formatters,
  182 linters + 2 internal, 52 DAP modules; diagnostics on
  `lua/games/aseprite/actions.lua` attached lua_ls + stylua_ls,
  settled, 0 diagnostics.

## Bugs found and fixed during the build

1. `v:vimentered` does not exist in Neovim (E121) — readiness probe
   is now `vim.fn.has('vim_starting')`.
2. rmpv `Utf8String::into_str`/`as_str` return `Option` — codec and
   error rendering adjusted.
3. tokio needed the `io-std` feature for stdin/stdout.
4. Protocol tests hung at teardown because `tokio::io::split` halves
   did not propagate EOF; harness now uses two one-directional
   duplex pairs.
5. Attach-mode connect now fails fast when the socket's parent
   directory does not exist instead of burning the 10 s deadline.
6. First-call LSP empties on cold servers fixed by progress-gated
   polling inside the Lua tools (`$/progress` tokens on
   `client.progress`), with a 30 s request-phase budget.

## State

- Binary installed: primo `~/.local/bin/periscope` (1,587,480 bytes).
- Source: sandbox `~/workspace/periscope/` (git, local only);
  primo copy at `~/scratch/periscope-src/`.
- NOT wired into the live diver config: the `declared_servers`
  snippet is in the README for Matt to approve.

## P2 — formatter/linter execution (2026-10-11, ~15:40 PDT)

Tools added: `run_format`, `run_lint` (12 tools total).

- `run_format` drives the config's own pipeline
  (`require('formatters').format`, async=false, explicit timeout):
  optional explicit formatter names (≤16, validated charset) and
  LSP mode (fallback/never/only; names+only rejected Rust-side,
  mirroring the module's own assert). Returns the module's status
  verbatim (ok/unavailable/disabled/stale/error/timeout), `changed`,
  the steps that ran, and the post-format buffer text (1 MB cap,
  UTF-8-safe truncation). The pipeline applies to the brokered
  editor's buffer only; the file on disk is never written
  (`written: false` in every result; smoke verified sha256 of the
  targets before/after).
- `run_lint` drives `require('linters').run_linter` per linter
  (manual mode, per-linter 50 s timeout, 60 s overall budget) and
  reports the module's own on_complete contract: status, verified,
  exit code, reason, and parsed diagnostics normalized to the P1
  shape with `source` = the producing linter. Default selection is
  the module's `linters_by_ft[filetype]` (+ `['*']`). A run that
  reports 'stale' (buffer changed under it — his own pipelines
  converge by re-running) is retried exactly once inside the same
  budget.
- Gates: sandbox + primo cargo test 18/18 (2 new protocol tests:
  round-trip against the fake editor; six adversarial argument
  cases rejected before any Lua runs); clippy -D warnings clean;
  fmt clean; primo IDE gate 0 diagnostics.
- Live smoke (primo, live config, managed mode): ugly.rs → rustfmt
  applied, changed, correct text; repeat explicit run →
  changed=false (idempotent); ugly.lua → stylua applied;
  plain.txt with lsp=never → 'unavailable' with the module's own
  message. dirty.lua default set → luacheck W211+W113 and selene
  unused/undefined diagnostics, each attributed; broken.sh →
  shellcheck ERRORs, exit 1; clean.sh → shellcheck ok/verified.
  Bogus linter name → per-linter 'unavailable: linter is not
  registered', not a crash.
- Faithful-config notes: luacheck/shellcheck report
  'unavailable: executable is not installed' when the broker is
  spawned without ~/.local/bin on PATH (the MCP client's
  environment decides; under diver it is complete). His shellcheck
  adapter passes --severity=warning, so info-level SC2086 is
  correctly not surfaced.
- Binary reinstalled: primo ~/.local/bin/periscope (1,626,112 B).

## Closeout audit (2026-10-12 overnight)

- Wiring state (corrects the Slice-1 "State" note above): periscope
  IS wired into the live diver config, and the entry was mirrored to
  the diver repo at `f823811f` (2026-10-11, on Matt's "move it");
  Matt validated tools/list and tools/call firsthand, in the terminal
  and in Neovim. The installed primo binary is still the P2 build
  (1,626,112 B, 12 broker tools).
- MATE attention extension (`8220510`, 2026-10-12): vendored
  joint-attention board + the four `attention_*` tools (16 total).
  NOT landed: that build is not installed on primo. Installing it
  changes the live surface from 12 to 16 tools and remains a separate
  decision, as `8220510`'s own message reserved.
- Defects found by this audit — the committed tree did not pass its
  own gates: (1) `src/attention.rs` did not compile: the board lock in
  `attention::call` was not bound `mut` while
  `FocusBoard::publish` takes `&mut self` (E0596); (2)
  `cargo fmt --check` failed on `src/attention.rs`; (3)
  `tests/protocol.rs` `tools_list_is_complete_and_sorted` still
  expected the 12 broker tools; (4) the README lacked the
  `run_format`/`run_lint` rows (an edit left uncommitted in both
  working trees) and any attention documentation, and its
  security-posture paragraph still claimed a read-only v0.1 that
  "never formats"; (5) this record's state picture stopped at P2.
- Fixes (the closeout commit after `8220510`): the `mut` binding;
  `cargo fmt`; the protocol expectation extended to the sorted 16;
  README completed (P2 rows, attention rows + board paragraph,
  security posture corrected); this section.
- Gates after the fix, on BOTH machines (sandbox + primo
  `~/workspace/periscope-mate`, whose attention sources are
  byte-identical to the sandbox commit): `cargo fmt --check` clean;
  `cargo clippy --all-targets -- -D warnings` clean; `cargo test`
  22/22 (4 lib + 5 broker + 1 lua-syntax + 12 protocol). Hermetic
  stdio smoke: initialize → tools/list returns the 16 sorted names;
  attention publish/focus round-trips with no editor involved; a
  publish spoofing another agent_id is refused in-band.
- Copies: the sandbox repo is canonical and local-only (no remote is
  configured — there is nothing to push). primo-mate's uncommitted
  attention WIP was the origin of `8220510` and now carries the same
  fixed files, hash-verified.
