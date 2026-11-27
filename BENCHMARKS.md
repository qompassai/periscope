# Benchmarks — periscope

Measured 2026-11-27 on primo with the bench in `benches/hot_paths.rs` and the example in `examples/stdio_session.rs`.

## Machine

- primo — 13th Gen Intel Core i7-13620H, 16 threads, 62 GiB RAM
- cargo 1.100.0 / rustc 1.100.0 (repo toolchain pins apply where present)
- Builds run with `nice -n 10`, `CARGO_BUILD_JOBS=4`
- Note: primo was shared with other batch builds during the 2026-11-27 run, so absolute numbers carry load variance; treat them as indicative, not laboratory-grade.

## Workloads (`benches/hot_paths.rs`, hand-rolled on `std::time`, `harness = false`)

- `cold_start_discover` — spawn the server binary and complete one `server/discover` request (modern MCP era, protocol 2026-07-28); n = 15 (`COLD_START_RUNS`).
- `tools_list` — `tools/list` on one warm session, after 20 warmup rounds; n = 200 (`LIST_CALLS`).
- Representative tool call — the cheapest read-only tool (below) on the same warm session; n = 200 (`TOOL_CALLS`).

Run them yourself: `cargo bench --bench hot_paths`. Run the session example: `cargo run --example stdio_session` (after a plain `cargo build`, so the server binary exists).

Representative tool: `attention_publish` — the server runs in attach mode against a nonexistent socket (`--attach /nonexistent/periscope-bench.sock`); the attention board is in-process and never touches an editor, so no Neovim is needed. Each publish advances `seq` by one (anti-replay rule).

## Results (2026-11-27 run)

| Workload | ns/op | ops/s |
|---|---|---|
| cold_start_discover | 51,340,831 | 19.5 |
| tools_list | 1,501,795 | 665.9 |
| attention_publish | 1,112,206 | 899.1 |
