# Permission model

In headless mode, agy soft-denies every command for which it has no rule. When soft-denied, the turn terminates without a response, causing delegated tasks to be lost without any denial notification.

## Three postures

| Posture | Permissions granted to delegated runs | Behavior in your own agy sessions | Trade-off |
|---|---|---|---|
| Default | Pre-granted `regex:` commands plus in-budget auto-grant recovery | Unchanged interactive review prompts (`request-review`) | Unrecognized or wrapped commands fail the turn with no response |
| `AGY_PERMISSIONS=trusted` | Auto-approves all tools (`--dangerously-skip-permissions`) | Unchanged interactive review prompts | Disables all safety checks for delegated sub-agent runs |
| Guard (`install-hook`) | Fast execution with a safety denylist; denials produce a one-sentence report instead of killing the run | Passes through with ~20ms overhead; interactive confirmation unchanged | Requires setting `toolPermission: always-proceed` globally |

## Inspecting configuration state

Because posture spans four files managed across three programs, inspect it directly rather than guessing:

```powershell
agy-mcp setup show
```

This outputs the currently active posture, agy gate state, whether the guard is registered and its binary exists, and configuration for each client registering `agy`. It also flags configurations that look configured but do nothing — installing the guard while the gate remains enabled means it is never consulted, and adding `-e` to a client other than the launching one has no effect. Both were observed during development of this feature.

## Enabling the guard

The quickest way is `agy-mcp setup apply guarded`. It performs both steps below idempotently. It can be reverted with `agy-mcp setup remove`. To do it manually, follow these two steps in order:

1. Register the hook:
   ```powershell
   agy-mcp install-hook
   ```
2. Set `"toolPermission": "always-proceed"` in `~/.gemini/antigravity-cli/settings.json` (or press `Shift+Tab` in an interactive agy session to switch modes).

Order matters: if the gate remains enabled, agy soft-denies before the guard is ever consulted.

Neither step requires restarting the client, because agy reads `hooks.json` on startup and the bridge spawns a fresh agy per run. `AGY_PERMISSIONS=trusted` is the opposite: because it is held in MCP registration details, it takes effect only when the client next launches the server — and because Claude Code and Claude Desktop are registered separately (`~/.claude.json` and `%APPDATA%\Claude\claude_desktop_config.json`), you must add `-e` to whichever client you actually use.

## What the guard refuses

The guard refuses commands that execute arbitrary strings as code (`iex`, `invoke-expression`), machine and OS configuration tools (unless running read-only inspection subcommands: `shutdown`, `restart-computer`, `set-executionpolicy`, `reg`, `netsh`, `bcdedit`, `diskpart`, etc.), recursive or forced deletion (`rm -rf`, `Remove-Item -Recurse -Force`), operations reaching outside the checkout (publishing to a registry, credential management, PATH installs like `cargo install` or `npm i -g`, `docker push`, `gh auth`), rewriting history or discarding uncommitted work (`git push`, `git reset --hard`, `git clean -f`), and file writes targeting outside authorized workspace roots. This is a denylist approach; anything not on the list passes through.

## Not applied to your own sessions

Because hook configuration is shared with desktop apps and IDEs, the guard identifies bridge-mediated runs via `AGY_MCP_DELEGATED=1` and passes all other executions straight through. Measured overhead in interactive sessions is about 20ms.

## Tuning rules

The guard reads rules from `~/.agy-mcp/policy.toml` and `<workspace>/.agy/policy.toml` as lists of `deny` and `allow` prefixes:

```toml
deny = ["flyctl deploy"]
allow = ["git push"]
```

The `allow` key always takes precedence over `deny`. Furthermore, dispatching agents can supply per-run rules through preset `policy_deny` and `policy_allow`.

## Known limitations and caveats

- **Fail-closed:** If the hook handler binary is missing or cannot be executed, agy treats the failure as fail-closed and blocks all tool calls — including your own sessions, because hook configuration is shared. For this reason, `install-hook` registers a dedicated copy at `~/.agy-mcp/guard/agy-mcp-guard.exe` rather than the installed binary. This prevents `cargo build` or `install.ps1` from rendering sessions unusable. Re-run `install-hook` after upgrades to update this copy.
- **Over-splitting during inspection:** The guard inspects every fragment of a command line across delimiters and substitutions. As a result, a shell write whose content mentions a denied command is interpreted as if it were the command itself.

## Disabling the guard

To uninstall the guard:

```powershell
agy-mcp install-hook --uninstall
```

After removing the hook, restore `"toolPermission"` to `"request-review"` in `~/.gemini/antigravity-cli/settings.json`.

Removing the hook also deletes the pinned copy.

If an execution has failed and you are unsure whether permissions are the cause, refer to [`troubleshooting.md`](troubleshooting.md). You can check the currently active permission mode (normally invisible from the outside) via `agy_diag(action=status)`.
