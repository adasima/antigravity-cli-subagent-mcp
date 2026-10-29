# antigravity-cli-subagent-mcp

An **MCP server** that lets Claude Code, Claude Desktop, or any MCP host hand a task to Google's
**Antigravity CLI (`agy`)** as a sub-agent, and get the sub-agent's report back.

It drives the official `agy` binary you installed and signed in to. It never reads, stores, or
forwards your credentials. One Rust binary, `agy-mcp`, no runtime to install.

It is a Rust reimplementation and expansion of the approach pioneered by
[SinanTufekci/Claude-Code-Antigravity-CLI-MCP-Server](https://github.com/SinanTufekci/Claude-Code-Antigravity-CLI-MCP-Server)
(Python). Look there if you want one bridge across many backends; this one goes deep on making a
single backend reliable.

## Before you use it

- **Not affiliated with Google or Anthropic.** "Antigravity" and "Gemini" are trademarks of
  Google LLC; "Claude" is a trademark of Anthropic.
- **Check the Antigravity terms for your account.** Google's
  [Antigravity FAQ](https://antigravity.google/docs/faq/) says headless mode (`agy -p`) is meant
  for direct scripting and CI, and that wrapping `agy` inside third-party coding agents is not
  supported under consumer plans. Read it and the
  [Terms of Service](https://antigravity.google/terms/), and decide for your own account.
- **Built for a person at the keyboard.** It has no daemon mode, no network endpoint, and no
  account switching, and it is not meant to run an unattended agent around the clock.
- **The sub-agent acts on its own.** `agy -p` reads and writes files, runs shell commands, and
  reaches the network without asking. Use trusted prompts on trusted content (see
  [Security](#security)).

## Requirements

- Windows. The server, the installer, and the tests are developed and run on Windows 11.
- The **Antigravity CLI**, signed in: `irm https://antigravity.google/cli/install.ps1 | iex`,
  then run `agy` once and sign in. Tested with agy 1.4.
- **Rust** (cargo) to build.

## Install

```powershell
cargo install --git https://github.com/adasima/antigravity-cli-subagent-mcp
claude mcp add agy -s user -- agy-mcp
```

Then **restart your client**. `agy` is found at `%LOCALAPPDATA%\agy\bin\agy.exe` (else on PATH);
add `-e AGY_BIN="<path to agy.exe>"` only if yours is elsewhere. For a host that takes JSON:

```json
"agy": { "command": "agy-mcp" }
```

Other hosts (Claude Desktop, Antigravity itself) and every setting are in
[`REFERENCE.md`](REFERENCE.md).

## First call

```
agy_sub(prompt="Summarize what this repo does and how it is laid out.", workspace="C:\\path\\to\\repo")
```

You can omit `workspace` if your client offers exactly one MCP root. The reply is the
sub-agent's report plus a footer:

```
↳ 7 steps · wrote: src/foo.rs · ~12k ctx · agy_diag(trace) for detail
```

`wrote:` comes from the transcript and the workspace's real `git diff`, not from the sub-agent's
own account of what it did.

## The three tools

| Tool | Purpose |
|---|---|
| `agy_sub` | Hand over a task. `model` = `flash` (default) / `pro` / `sonnet` / `opus` or a model name. `conversation_id` continues an earlier run. |
| `agy_poll` | Collect a run whose result did not come back with the call. |
| `agy_diag` | Diagnostics, no quota: `status`, `mcp`, `trace`, `sessions`, `live`. |

## Good to know

- **A call can return a job id instead of a report.** Some clients cut long requests, so the
  server answers early with a job id and keeps the run going. Collect it with `agy_poll`, or from
  a terminal with `agy-mcp wait` (blocks until done, costs no tokens).
- **Runs go in parallel**, up to `AGY_MAX_CONCURRENT` (default 10).
- **Presets save typing.** `preset=code` (edit, short reply), `research` (read-only),
  `report` (five-line reply). A project can add its own in `.agy/presets.toml`.
- **The workspace is never guessed** from the process's current directory: explicit arg, then
  your client's single MCP root, then `AGY_WORKSPACE`, else an error.
- **Permissions are the one setting worth a decision.** Headless `agy` quietly refuses a command
  it has no rule for, and the run ends without an answer. The defaults cover builds and tests;
  [`docs/permissions.md`](docs/permissions.md) explains the options.

## Security

`workspace` is a starting directory, not a sandbox. For real isolation run this in a container or
VM, and review changes with `git diff`. The server never passes `--dangerously-skip-permissions`
unless you opt in at setup time; see [`docs/permissions.md`](docs/permissions.md).

## Docs

| File | For |
|---|---|
| [`REFERENCE.md`](REFERENCE.md) | Every tool, argument, setting, and message |
| [`docs/troubleshooting.md`](docs/troubleshooting.md) | Symptom, cause, and the command that fixes it |
| [`docs/permissions.md`](docs/permissions.md) | Choosing a permission setup |
| [`docs/architecture.md`](docs/architecture.md) | How the code is built, module by module |
| [`skills/agy-guide/`](skills/agy-guide/SKILL.md) | A Claude Code skill on writing good delegations |

## Issues and pull requests

Issues are welcome. Pull requests are not accepted: this repository is a periodic export of a
private working tree, so a pull request has nowhere to merge. Describe the change in an issue
instead.

## License

[MIT](LICENSE)
