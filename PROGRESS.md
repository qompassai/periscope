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
