# Dispatch — from the call to the report

## Normally there is nothing to do

`agy_sub` returns the finished report. No job id to carry, no follow-up command. Dispatch it like
any other tool and don't reason about how long the task will take beforehand.

## When it doesn't come straight back

**Some clients bound a single request.** Claude Desktop cuts a call at ~60s, so on that surface
the bridge acks at 45s rather than letting the call be killed. On a client without that bound you
just get the result, however long it took.

**When you get an ack, run `agy-mcp wait <job_id>` as a background Bash task in the same turn.**
It blocks on disk, costs zero tokens, and returns the full report in one round-trip. Treat
`NEXT ACTION REQUIRED` as literal — a job was once started, the wait was never run, and the
session ended before anyone came back. The run finished fine; nobody looked.
Once the wait runs, leave it: reading its empty output file or calling `agy_poll` meanwhile only
spends calls (`agy_poll` itself says a wait is watching). You are woken when it ends; with nothing
else to do, end the turn saying you're waiting on the job — not "waiting…", which reads as stuck.

**`Request timed out` is not the run failing.** The client gave up on the *request*; the job is
detached and keeps going. Same recovery: `agy-mcp wait <job_id>`, or with no argument at all.
Don't re-dispatch.

**Safety net:** any later `agy_sub` or `agy_diag(status)` prepends `⚠ uncollected background job
result(s)` if something finished and was never retrieved, for up to 30 min. `agy-mcp wait` with
no argument picks up exactly that set.

## A run survives the client, not the server

Losing the request never loses the work. But if the **agy-mcp server** dies (Claude Code restart,
machine restart) the run dies with it and its record is demoted to failed. What you get back then
is a **resume line**: the record keeps the conversation id, the workspace, the full prompt, and
what the run was last doing (`last seen: <last_step>`), so continue with
`agy_sub(conversation_id=…, prompt="Continue the task from where it stopped.")` — that keeps
whatever the sub-agent already did. `agy-mcp show <job_id>` prints the verbatim dispatch spec.
**Never reconstruct a lost prompt by hand.**

## The CLI — `wait` / `poll` / `jobs` / `show` / `doctor`

`agy-mcp` is the same binary as the MCP server (on PATH via `~/.cargo/bin`); a subcommand makes it
a one-shot CLI over the disk mirror in `~/.agy-mcp/jobs/`. No MCP, no agy launch, no quota, no
tokens.

- **`agy-mcp wait [<id>…]`** — blocks until the job(s) finish and prints each report. Exit 0 all
  done / 1 any failed / 2 timeout. **Without `--timeout` it keeps waiting until each job is past
  its own run ceiling (`AGY_TIMEOUT`, default 3600s, + 600s from its start)** — an explicit
  `--timeout <secs>` keeps the fixed deadline behavior. **With no id it waits for everything the
  current workspace is still owed** — running jobs plus uncollected results — so it is the recovery
  path when an id scrolled out of context. Scope comes from the cwd, so run it in the project
  directory; `--all` drops the scope. It waits for **all** targets and prints the roster it resolved.
- **`agy-mcp poll <id>`** — one-shot status. `agy_poll` over MCP is fine for a single quick check.
- **`agy-mcp jobs`** — the roster.
- **`agy-mcp show <id>`** — the saved dispatch spec: full prompt, workspace, conversation id.
- **`agy-mcp doctor`** — answers "why do I have no `agy_*` tools?" (live server pids, agy binary,
  job state). Servers are per-session and coexist, so a missing toolset means *your* session's
  server didn't start. **Dispatch is MCP-only by design** — never hand-roll an MCP client against
  the binary, and don't silently absorb the work in-session without saying so.

## There is no timeout argument

A run is not governed by a fixed total wall-clock. The bridge watches agy's liveness and stops it
only after **`AGY_IDLE_TIMEOUT` seconds with no activity (default 180s)**, bounded by a generous
ceiling (`AGY_TIMEOUT`, 3600s). This is why a long `cargo build` the sub-agent is waiting on no
longer trips the timeout.

- **Idle is what kills a stalled run.** `⚠ NO agy activity for Ns` past 180s means dead, not busy
  — stop waiting and resume the conversation instead.
- **A `hard cap` timeout means the run was still active at the ceiling.** Split the task.
- **A failed run reports what it finished first.** The error leads with `before the failure: N
  steps · wrote: <files> · error: <agy's own receipt>` and, in a git workspace, `changed on disk:
  <paths>`. Read the partial progress off the failure instead of assuming nothing happened, then
  **resume the same `conversation_id` to continue, not restart**.
- **An empty-transcript error is not always trust/OAuth.** The bridge reads agy's own logged
  receipts; a 429 is reported as a quota condition explicitly.
- **An unexplained `exit 1` right after handing over an image** means the image is corrupt or an
  unsupported format — see `multimodal.md`.

## Resuming vs fresh

- `conversation_id` resumes (appends to the same session); omit it to start clean. Use
  `agy_diag(action=sessions)` to find ids and their ~ctx size, and weigh reuse (keeps context)
  against a fresh run (smaller, cheaper, less risk of a bloated session).
- **Switching model on resume.** Omit `model` to keep the conversation's own model. Pass a `model`
  *with* `conversation_id` to **switch and continue** the same thread, without paying to
  re-establish context.
- **Resuming the same conversation concurrently is safe** — it never branches or errors.
- **A prefix is enough** for `conversation_id`; the bridge resolves it.

**Context is reported, never enforced.** Past ~100K ctx a flash run comes back with
`⚠ this conversation is now ~Ntok …` — before the answer on a resume, after it when the run itself
grew that big. **Nothing is refused and nothing is killed**; you get the work *and* the sentence.
Act on it when answers start drifting; ignore it when the task is done. `force=true` silences it.
**It does not name a model to switch to**, on purpose: every line degrades on long contexts, so
switching is not the fix. Start a fresh conversation with a short handoff summary instead.

## Workspace

- **Pass `workspace=<project root>` explicitly** for any file/build task. Running in a separate
  worktree or scratch dir means every build is **cold and slow** — a gitignored `target/` isn't
  shared.
- The bridge is cwd-less: it never guesses from the process cwd. It *can* ask your client for its
  MCP roots when you omit the arg, but only when the client offers exactly **one**.
  **`agy_diag(action=status)` tells you which case you are in by running the resolution itself** —
  check it in the session you are in, not from memory. The same client has advertised roots and
  then handed back none. Antigravity (GUI, IDE, CLI) speaks the 2026-07-28 protocol, which has
  no roots: from there, always pass `workspace`.
- **`agy_sub` never silently falls back to a scratch dir.** If nothing resolves you get an error,
  not a sub-agent burning quota hunting for a project that isn't there.
- **Isolation is opt-in.** Pass an explicit alternate `workspace` only when you specifically want
  the agent walled off from the working tree. Don't make a worktree the default — that is exactly
  what makes builds slow.
- **Per-workspace skills and MCP servers are agy's own feature, and they load headlessly.** Put
  them in `<workspace>/.agents/skills/<name>/SKILL.md` and `<workspace>/.agents/mcp_config.json`
  (same format as `~/.gemini/config/`). No `AGENTS.md` pointer is needed.
- **Quote the `description` in a SKILL.md frontmatter.** agy's YAML parser drops a skill whose
  unquoted value contains `: `, and says so only in its own log — Claude Code accepts the same
  file. `agy_diag(action=status)` names the skills that will not load.
- `mcp=none` / a list scopes the global config only; a workspace's own servers stay on.

## A sub-run does not delegate

A sub-run's agy starts agy-mcp from its config, and that copy exits at once: **no `agy_sub` from
inside a delegated run.** Don't plan an order around it. Split the work yourself, or let the
sub-run use agy's own sub-agents (same model).

## Fanning out

Runs go in parallel, up to **10 concurrent** (`AGY_MAX_CONCURRENT`). Issue several `agy_sub` calls
in one turn; your client sends them concurrently and each carries back its own answer, so a
fan-out never waits on its slowest member. Diagnostics stay responsive throughout.

- A run with a custom `mcp` scope fans out like any other; only its spawn window is serialized.
- Runs on the same quota group share it: fanning out 10× flash spends the GEMINI bucket 10×
  faster. The Claude group (sonnet/opus) is a separate bucket (`quota.md`).
- **Parallel runs share one working tree.** Say what each one may not touch (see the order's
  constraints section) — this is where that rule earns its keep. A lane that isn't told about
  another sees "files I didn't change" and reverts them; one build picks up another lane's
  half-written file. When a dispatch lands next to a running job, the reply opens with
  `⚠ same working tree as running job(s): jN "…" (wrote: …)` — if the orders didn't already
  fence each other off, fix that now, or move one lane to its own worktree (and its own
  `--target-dir`).
