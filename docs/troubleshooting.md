# agy-mcp troubleshooting

This guide is for users who have installed agy-mcp and encountered problems during use.
If you are developing or contributing to the codebase, refer to `AGENTS.md` instead.

---

## `agy_*` tools do not appear in the session

**Symptom** `mcp__agy__agy_sub`, `mcp__agy__agy_poll`, and `mcp__agy__agy_diag` are missing from the Claude Code tool list. Calling them returns a "tool not found" error.

**Cause** Claude Code starts one MCP server per session. If that server fails to start, or if the session was started before `agy-mcp` was registered, the tools simply remain absent for the lifetime of that session. There is no way to recover from within the session, because dispatch occurs exclusively via MCP and the CLI has no dispatch path.

**Remedy** Run the doctor command to see what is visible from the server:

```
agy-mcp doctor
```

Running server PIDs, the agy binary path, and job mirror status are output.
If no servers are displayed, restart Claude Code (servers are started per session).
If the tools still do not appear after restarting, verify the registration status:

```
claude mcp list
```

If `agy` is not displayed, re-register it:

```
claude mcp add agy -s user -- agy-mcp
```

Then restart Claude Code again.

---

## "workspace is required" reported / two roots offered

**Symptom** `agy_sub` returns an error stating that `workspace` is required, or lists two (or more) project roots and demands an explicit selection.

**Cause** `agy_sub` never falls back to a scratch directory — doing so would burn quota hunting for a nonexistent project. The workspace is resolved in this order: an explicit `workspace` argument → the client's single MCP root → `AGY_WORKSPACE` → error. If the client offers multiple roots, the bridge does not arbitrarily pick one; it lists all of them and stops.

The same resolution logic is used by `agy_diag(action=status)`, which prints either the resolved path or the exact reason for failure — if you are unsure what is visible from this client, run that first.

**Remedy** Pass the path explicitly:

```
agy_sub(prompt="…", workspace="/absolute/path/to/your/project")
```

Or configure `AGY_WORKSPACE` at registration time:

```
claude mcp add agy -s user -e AGY_WORKSPACE=C:\path\to\project -- agy-mcp
```

---

## Execution stopped on permission denial

**Symptom** The response contains a `DeniedError` naming a specific command or file path. The footer displays `before the failure: N steps · wrote: …` along with the work already completed.

**Cause** Because `agy -p` runs headlessly, it cannot display interactive approval prompts; commands outside pre-granted groups are soft-denied. Some commands are denied by design and cannot be auto-recovered: publishing to a registry, installing onto PATH, manipulating registry credentials, and wrapped commands (`powershell -Command "…"`, `cmd /c …`). Safe but unpermitted commands are auto-approved and retried up to 4 times per run (`AGY_AUTO_GRANT_BUDGET`, default 4). Once the budget is exhausted, execution stops.

**Remedy** Check whether the denied command falls into the "denied by design" category above. If it does, inspect the sub-agent's changes with `git diff`, then run the command yourself.

Safe commands outside pre-granted groups are recovered within the same run. The bridge records the grant, resumes that conversation itself, and indicates this in the response (`↻ auto-granted … after headless permission denial(s); resumed`). Action is required only when the budget has been exhausted, in which case you resume the conversation:

```
agy_sub(prompt="continue", conversation_id="<id from the footer>")
```

If execution frequently stops here, you should change the gate itself rather than the prompt. [`permissions.md`](permissions.md) describes the three postures (default gate, `AGY_PERMISSIONS=trusted`, and guard) along with their respective trade-offs.

---

## Call times out after about one minute with no response

**Symptom** On long-running tasks, Claude Desktop returns `Request timed out` around the 60-second mark. The execution itself is not lost.

**Cause** Claude Desktop (`local-agent-mode-*`) terminates a single MCP request after about 60 seconds — measured across 4 runs, unaffected by `MCP_TOOL_TIMEOUT`, `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS`, or progress notifications (measured 2026-08-15). The bridge is aware of this behavior and returns an acknowledgment (ack) with a job ID at 45 seconds on that surface, prior to termination. Because every execution is a detached job, work continues to completion regardless of what happens to the request.

**Remedy** Collect results with the `wait` command — it blocks until the job completes, reads the disk mirror, and prints the full report. Run it as a background task to avoid burning tokens on polling:

```
agy-mcp wait <job_id>
```

If no job ID is specified, it waits for all remaining jobs in the current working directory's workspace:

```
agy-mcp wait
```

Exit codes: `0` = all succeeded, `1` = any failed, `2` = timed out / unknown ID / nothing to wait for.

---

## Job stays shown as running with no progress

**Symptom** `agy_poll` or `agy-mcp jobs` reports the job as `running`, but displays a `⚠ NO agy activity for Ns` warning, or the job has been running much longer than expected without updates.

**Cause** The bridge tracks liveness through SQLite WAL files (`<id>.db-wal`), which agy updates on every turn. If liveness stalls (agy stalls, machine suspends, or the owning server exits), the idle timeout (`AGY_IDLE_TIMEOUT`, default 180s) stops agy, and the job is recorded as failed. Wall-clock time (not monotonic timers) is used for all limits, so machine suspension does not pause the idle counter.

Jobs whose owning server has terminated are automatically detected and downgraded to failed the next time any `agy-mcp` command runs. The job record preserves recovery hints, including the full prompt and a resume command line.

**Remedy** Check current status:

```
agy-mcp jobs
```

If the job is recorded as failed, retrieve the recovery specification:

```
agy-mcp show <job_id>
```

The output includes the full prompt and a ready-to-run resume command line.

---

## 429 / quota error returned

**Symptom** Execution fails with a message containing `[429]`. The footer may show `before the failure: 0 steps`.

**Cause** The AI Pro/Ultra weekly or 5-hour quota for that model group has been exhausted. Receiving a 429 is itself definitive quota information. The bridge immediately records a 0% snapshot for that group and pre-emptively refuses the next dispatch to avoid wasteful round-trips.

**Remedy** Check the cached quota status (no scraping, instant):

```
agy-mcp quota
```

To fetch the latest state from the "Models & Quota" page (~10-15 seconds):

```
agy_diag(action=live, what=quota)
```

Switch to another model group that has quota remaining:

```
agy_sub(prompt="…", model="sonnet")
```

The 5-hour window recovers automatically over time. Quota information accumulates automatically, so polling is unnecessary. When the server is idle and more than 30 minutes have elapsed since the previous fetch, the bridge automatically re-scrapes after a run.

---

## agy transcript is empty or has no content

**Symptom** Execution terminates but the response is empty, or the bridge reports an empty transcript. Work output may still exist on disk.

**Cause** There are two distinct causes for this symptom:

1. **Untrusted workspace:** On the first run, agy displays a "trust this folder?" prompt, which cannot be answered in headless mode. The bridge auto-trusts workspaces before each run, but if that step fails, the execution terminates with exit 0 and an empty transcript.

2. **Workspace on a different drive on Windows:** In one observed incident (2026-06-25, agy v1.0.11), for a workspace on a drive different from the home directory, agy wrote the transcript to a path without a drive letter, preventing the bridge from finding the response. The bridge sets the child process working directory to the workspace, and this has not recurred in current agy versions — though it has not been re-measured either, so please report if an empty transcript occurs with a cross-drive workspace. In either case, file edits made by the model are reflected on disk as usual, so check `git diff` before assuming no work was done.

**Remedy** Check whether work output is already on disk:

```
git diff
git status
```

If the workspace is not yet trusted, launch agy once interactively in that directory to answer the prompt, then retry:

```
agy
```

To verify that the cross-drive workaround is active, run:

```
agy_diag(action=status)
```

---

## Footer reports no changes / "may have punted"

**Symptom** The end of the response displays `⚠ this code task reported done but … — it may have punted`. The footer `wrote:` list is empty.

**Cause** This warning fires only under `preset=code` (and presets that extend it) when an execution ends without leaving any changes. It indicates an actual failure mode: the sub-agent may have stopped after preparatory steps without writing the actual implementation. If the workspace is a git repository, this check is based on working tree diffs — not merely the absence of descriptions in the transcript, but a clean tree itself serves as evidence.

**Remedy** Inspect the trace to see what the sub-agent actually did:

```
agy_diag(action=trace)
```

Then resume with a more specific prompt:

```
agy_sub(prompt="the implementation was not written — please write it now",
        conversation_id="<id from the footer>")
```

---

## Could not resolve model slot

**Symptom** `agy_sub` returns an error stating that the requested slot could not be resolved, and offers `agy_diag(action=live, what=models)` as a manual verification path.

**Cause** The bridge resolves model slots (flash / pro / sonnet / opus) from a local cache (`~/.agy-mcp/models.json`). If the cache exists but the requested slot is missing (such as when Google updates available model versions), the resolver re-fetches the list once using the headless `agy models` subcommand (zero quota, ~2s) and retries automatically. Only misses that survive a fresh list are reported as errors.

**Remedy** Update the cache manually:

```
agy_diag(action=live, what=models)
```

If the alias still does not resolve after updating, specify the name directly from the list (bypassing the alias):

```
agy_sub(prompt="…", model="gemini-3.8-flash")
```

The list of names can be checked in the output of `agy_diag(action=live, what=models)`.

---

## When the issue persists

Run self-diagnostics to understand the current state of the bridge:

```
agy-mcp doctor
```

Next, call the diagnostic tool to check real-time state from within the session:

```
agy_diag(action=status)
```

`doctor` displays running server PIDs, the agy binary path, and job mirror status.
`agy_diag(action=status)` displays the agy version, resolved workspace, active permission mode, and available presets — bringing together the most confusing causes in one place.
