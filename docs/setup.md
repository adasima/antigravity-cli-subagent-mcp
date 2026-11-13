# Setup guide — Claude Code, Claude Desktop, Antigravity, and other hosts

This guide covers installing `agy-mcp`, registering it across each MCP host, linking the `agy-guide` delegation skill, and configuring setup-time environment variables. For permission postures (`gate`, `trusted`, `guarded`), see [`permissions.md`](permissions.md). For runtime behavior of each tool, see [`../REFERENCE.md`](../REFERENCE.md).

---

## 1. Prerequisites and binary installation

### Antigravity CLI (`agy`)

Install the official Antigravity CLI and sign in interactively once with your Google AI Pro or Ultra account:

```powershell
irm https://antigravity.google/cli/install.ps1 | iex
agy
```

Headless runs (`agy -p`) cannot open a browser window; if your OAuth session expires later, launch `agy` interactively once to refresh it.

### Installing `agy-mcp`

**From GitHub (end users):**

```powershell
cargo install --git https://github.com/adasima/antigravity-cli-subagent-mcp
```

**From a local clone (development / updates):**

```powershell
.\install.ps1                  # build, swap ~/.cargo/bin/agy-mcp.exe, register Claude Code, link Claude skill
.\install.ps1 -Stop            # also stop idle agy-mcp servers still holding the previous binary
.\install.ps1 -AntigravitySkill # also link agy-guide globally into ~/.gemini/config/skills
```

> **Why `~/.cargo/bin` instead of `target\release`:** A running MCP server holds an exclusive lock on its executable. `install.ps1` builds in `target\release` (never blocked), renames the live `~/.cargo/bin/agy-mcp.exe` aside to `agy-mcp.exe.old`, and copies the new binary in place. Running servers keep working until you restart their host app (or pass `-Stop`, which stops only servers that do not own an active job).

---

## 2. Host-by-host MCP registration

### A. Claude Code (CLI)

Claude Code stores user-scope MCP registrations in `~/.claude.json` and spawns one server process per session.

```powershell
claude mcp add agy -s user -- agy-mcp
```

To pass setup-time environment variables, include `-e` flags at registration time:

```powershell
claude mcp remove agy -s user
claude mcp add agy -s user -e AGY_PERMISSIONS=trusted -e AGY_DEFAULT_MODEL=flash -- agy-mcp
```

`install.ps1` leaves an existing `agy-mcp` registration untouched so that re-deploying a binary never drops your `-e` flags. After registering or updating, **restart Claude Code**.

### B. Claude Desktop

Claude Desktop reads `%APPDATA%\Claude\claude_desktop_config.json`, which is completely separate from Claude Code's `~/.claude.json` (registering both is normal when you use both apps; any `env` settings must be added to the file for the client you actually launch).

Add `agy` under `mcpServers`:

```json
{
  "mcpServers": {
    "agy": {
      "command": "agy-mcp",
      "args": [],
      "env": {}
    }
  }
}
```

**Inline wait behavior:** Claude Desktop (`local-agent-mode-*`) cuts off single MCP tool calls at ~60 seconds regardless of timeout settings. When `agy-mcp` detects this client, it automatically returns a job ID acknowledgment at **45 seconds** while the run continues in the background. Collect the result with `agy_poll` or `agy-mcp wait <job_id>`. Do not set `AGY_INLINE_WAIT=0` on Claude Desktop.

### C. Antigravity (GUI, IDE, and CLI)

Antigravity can act as the parent host that calls `agy_sub` to delegate tasks to sub-agent sessions.

- **Antigravity GUI / IDE config:** `~/.gemini/antigravity-ide/mcp_config.json`
- **Antigravity CLI config:** `~/.gemini/config/mcp_config.json`

Use the full path to `agy-mcp.exe` in `command` (GUI apps do not always inherit your shell's `PATH`):

```json
{
  "mcpServers": {
    "agy": {
      "command": "C:\\Users\\<you>\\.cargo\\bin\\agy-mcp.exe",
      "env": {}
    }
  }
}
```

Key behaviors when calling from Antigravity:

1. **Workspace is required:** Antigravity connects using the `2026-07-28` MCP discovery protocol (`server/discover`), which does not provide MCP roots. Always pass `workspace="C:\\path\\to\\repo"` on every `agy_sub` call, or set `"AGY_WORKSPACE": "C:\\path\\to\\repo"` in the `env` block above.
2. **150-second job handoff:** The Antigravity GUI (`antigravity-client`) cuts off MCP tool calls at 180 seconds. `agy-mcp` automatically returns a job ID at **150 seconds** so long runs never appear as transport errors; collect them with `agy_poll` or `agy-mcp wait <job_id>`.
3. **No nested process loops:** If you also register `agy` in the CLI config (`~/.gemini/config/mcp_config.json`), every delegated `agy -p` sub-run will attempt to start `agy-mcp`. The bridge marks every `agy` process it spawns (`AGY_MCP_DEPTH`), and any `agy-mcp` started underneath exits immediately before handshake, preventing recursion or stray processes.

### D. Other MCP hosts (Cursor, Windsurf, generic stdio clients)

Register `agy-mcp` as a standard stdio MCP server:

```json
{
  "mcpServers": {
    "agy": {
      "command": "agy-mcp",
      "args": [],
      "env": {}
    }
  }
}
```

- If the host provides **exactly one** MCP root via `roots/list`, `workspace` can be omitted in `agy_sub`.
- If the host provides **no roots** or **multiple roots** (multi-root workspace), pass `workspace` explicitly on each `agy_sub` call (or set `AGY_WORKSPACE` in `env`). Run `agy_diag(action="status")` from the host to see how `workspace` resolves in that session.

---

## 3. Delivering the `agy-guide` skill

Model selection, prompt structure, effort rules, and footer verification live in [`skills/agy-guide/SKILL.md`](../skills/agy-guide/SKILL.md) (with on-demand chapters under `skills/agy-guide/chapters/`) so they do not bloat the always-loaded MCP tool schema.

Always link the skill via a **directory junction** rather than copying it: junctions require no administrator privileges on Windows and never go stale when the repository updates.

You can manage skill links at any time **without building or touching the binary** using `scripts/link-skill.ps1`:

```powershell
.\scripts\link-skill.ps1 -Claude                    # ~/.claude/skills/agy-guide
.\scripts\link-skill.ps1 -Antigravity               # ~/.gemini/config/skills/agy-guide (global)
.\scripts\link-skill.ps1 -Claude -Antigravity       # both global locations
.\scripts\link-skill.ps1 -Workspace C:\path\to\repo # <repo>\.agents\skills\agy-guide
```

### Claude Code (global)

`install.ps1` links `~/.claude/skills/agy-guide` → `<repo>\skills\agy-guide` by default. Every workspace opened in Claude Code sees the skill automatically. Do **not** place a copy under a project's `.claude/skills/agy-guide` (project scope shadows the global junction with a snapshot that goes stale).

### Antigravity — Option A: Global (`~/.gemini/config/skills/agy-guide`)

To make `agy-guide` available in **every workspace** opened in Antigravity (matching the Claude Code setup), link it into Antigravity's global skills directory:

- **Standalone script (no build):**
  ```powershell
  .\scripts\link-skill.ps1 -Antigravity
  ```
- **During `install.ps1`:**
  ```powershell
  .\install.ps1 -AntigravitySkill
  ```
- **Manual PowerShell command:**
  ```powershell
  New-Item -ItemType Junction -Path "$HOME\.gemini\config\skills\agy-guide" -Target "<this-repo>\skills\agy-guide"
  ```

> **Note:** Because `~/.gemini/config/skills/` is shared with `agy -p`, delegated sub-runs also see the skill's two-line YAML frontmatter (~30 tokens; the body is loaded only on demand). Because delegated sub-runs have no `agy_sub` tool (nesting is disabled), they never activate it.

### Antigravity — Option B: Per-workspace (`<workspace>\.agents\skills\agy-guide`)

If you prefer not to place `agy-guide` in `~/.gemini/config/skills/`, link it only into the workspaces where you run an Antigravity parent session:

```powershell
.\scripts\link-skill.ps1 -Workspace C:\path\to\workspace
```

Or manually:

```powershell
New-Item -ItemType Junction -Path C:\path\to\workspace\.agents\skills\agy-guide -Target <this-repo>\skills\agy-guide
```

In a git-tracked workspace, `link-skill.ps1 -Workspace` automatically appends `.agents/skills/agy-guide` to `.git/info/exclude` so the local junction stays untracked.

---

## 4. Setup-time environment variables

Set these in your host's MCP registration (`claude mcp add -e KEY=VALUE` or the JSON `"env": { ... }` block). Restart the client after changing them.

| Variable | Default | Purpose |
|---|---|---|
| `AGY_BIN` | Auto-detected (`%LOCALAPPDATA%\agy\bin\agy.exe`, then `PATH`) | Full path to `agy.exe` if installed in a non-standard location. |
| `AGY_WORKSPACE` | Unset | Fallback workspace directory when `workspace` is omitted and the client offers no single MCP root. |
| `AGY_PERMISSIONS` | `gate` (unset) | Set to `trusted` to pass `--dangerously-skip-permissions` to every delegated run. See [`permissions.md`](permissions.md). |
| `AGY_DEFAULT_MODEL` | `flash` | Default model alias or name for fresh `agy_sub` calls that omit `model`. |
| `AGY_INLINE_WAIT` | Per-client (`45` on Claude Desktop, `150` on Antigravity GUI, `0` = wait until done elsewhere) | Seconds before `agy_sub` returns a background job ID (`0` disables early ack). |
| `AGY_MAX_CONCURRENT` | `10` | Maximum concurrent `agy` runs per server process. |
| `AGY_TIMEOUT` | `3600` | Absolute wall-clock ceiling (seconds) per run. |
| `AGY_IDLE_TIMEOUT` | `180` | Liveness inactivity timeout (seconds) before a silent run is terminated. |
| `AGY_VERIFY_TIMEOUT` | `120` | Wall-clock timeout (seconds) for a preset's post-run `verify` command. |
| `AGY_QUOTA_RETRY_CUTOFF` | `3` | Consecutive 429 retries in the transcript tail before early stop (`0` disables). |
| `AGY_MAX_ANSWER_CHARS` | `8000` | Maximum characters returned in the inline answer before truncation. |
| `AGY_PROGRESS` | `0` (off) | Set to `1` to emit MCP `notifications/progress` every 15s during in-flight runs. |

---

## 5. Verifying the installation

From a terminal:

```powershell
agy-mcp doctor        # checks agy binary, running server instances, active jobs, and default model
agy-mcp setup show    # inspects permission posture across Claude Code, Claude Desktop, and agy settings
```

From inside your MCP client session:

```text
agy_diag(action="status")
```

This confirms the connected `agy` version, the detected client name, how `workspace` resolves in the current session, the active permission mode, and loadable presets and skills.
