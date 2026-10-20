-- /qompassai/Diver/lua/ai/mcp/declared_servers.lua
-- Qompass AI Declared MCP Servers (Tiger Style)
-- Copyright (C) 2026 Qompass AI, All rights reserved
-- --------------------------------------------------
--
-- Version-controlled MCP server declarations. This file is the source of
-- truth for the fleet: it travels with the config, diffs in review, and
-- loads identically on every machine.
--
-- Secret-free by construction: any credential a server needs is a *reference*
-- (see ai.mcp.secrets), resolved at spawn time from pass, the environment,
-- or a command. Values never appear here, in the persisted registry, or in
-- logs. Each entry passes through registry.validate() on load; a bad entry
-- is reported and skipped, never fatal.
--
-- Fields per entry: name, command (absolute path), args (argv array),
-- env (non-secret variables only), secrets (variable -> ref), cwd,
-- enabled. Entries added via :McpAdd live in the persisted registry and win
-- over a same-named declaration.

return {
    {
        name = 'git',
        command = '/usr/bin/uvx',
        args = { 'mcp-server-git@2026.10.10' },
        enabled = true,
    },
    {
        name = 'fetch',
        command = '/usr/bin/uvx',
        args = { 'mcp-server-fetch@2026.10.10' },
        enabled = true,
    },
    {
        -- GUI-bridge mode (bethington/ghidra-mcp, installed): the bridge
        -- is a stdio MCP server that multiplexes to the GhidraMCP plugin
        -- inside a RUNNING Ghidra GUI (plugin HTTP on 127.0.0.1:8089 by
        -- default; GHIDRA_MCP_URL overrides). Bridge and plugin zip
        -- (/opt/ghidra/Extensions/Ghidra/GhidraMCP-6.0.0.zip) are already
        -- installed, and the bridge answers initialize/tools-list without
        -- the GUI -- but the 200+ proxied analysis tools need the plugin
        -- enabled in Ghidra with a program open. Headless analysis
        -- without the GUI is a different mode and lives in
        -- security.ghidra (:GhidraAnalyze).
        name = 'ghidra',
        command = '/usr/bin/bridge-mcp-ghidra',
        args = { '--transport', 'stdio' },
        enabled = true,
    },
    {
        -- Local binary (no docker): /usr/bin/github-mcp-server over stdio.
        -- The token is Matt's fine-grained PAT in pass at 'gh/pat', resolved
        -- at spawn time and never stored here. Full default toolsets: no
        -- --read-only flag and no toolset restriction args.
        name = 'github',
        command = '/usr/bin/github-mcp-server',
        args = { 'stdio' },
        secrets = {
            GITHUB_PERSONAL_ACCESS_TOKEN = { provider = 'pass', path = 'gh/pat' },
        },
        enabled = true,
    },
    {
        -- Periscope (Matt's own broker, built 2026-10-11): brokers this
        -- config's LSP, formatter, linter, and DAP surface over MCP.
        -- Managed mode (no args): the broker spawns its own headless
        -- Neovim with this config, rooted at the client's working
        -- directory. Format and lint act on buffers only; nothing is
        -- written to disk. No credentials required.
        name = 'periscope',
        command = '/home/phaedrus/.local/bin/periscope',
        args = {},
        enabled = true,
    },
    {
        -- REA (Reverse Engineer Anything, morluto/rea; Matt's fork
        -- qompassai/rea) v6.3.0, pinned: installed in user space with
        -- npm --prefix ~/.local, never @latest; `rea mcp` serves stdio.
        -- The 139-tool catalog spans native analysis (Ghidra/IDA/Hopper
        -- providers), JavaScript/Electron, managed code, and web
        -- observation, and sits behind the harness policy gate like
        -- every other declared server. No credentials required.
        name = 'rea',
        command = '/home/phaedrus/.local/bin/rea',
        args = { 'mcp' },
        env = { GHIDRA_INSTALL_DIR = '/opt/ghidra' },
        enabled = true,
    },
}
