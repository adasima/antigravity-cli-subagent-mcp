# Permissions — read the posture first, then write the prompt

**How a denied command behaves is not fixed; it depends on how this machine is set up**, and the
right prompt differs in each case. Check once per session:

```
agy_diag(action=status)      # the `permissions:` line
```

or, outside a session, `agy-mcp setup show`.

## `gate` (the default)

agy can't prompt in `-p` mode, so a command it has no rule for is **soft-denied, and a soft-deny
ends the turn with no answer**. Two layers keep that invisible for standard work:

1. **Pre-granted families, zero round-trips.** `cargo`, `npm|yarn|pnpm`, `npx`, `pytest`,
   `go test|build`, `dotnet test`, `mvn|gradle`, plus discovery (`where`/`which`,
   `<tool> --version`).
2. **Auto-recovery beneath it.** A line the patterns don't match is granted if it passes the
   safety policy, and the same conversation is resumed — **up to 4 times per run**, shown as
   `↻ auto-granted …`. Chains (`;`, `&&`, `||`) are decomposed and granted only when every segment
   is safe.

So write **which** build/test command to use — one standard invocation — not shell exploration.

**What it will not recover:** wrapped commands (`powershell -Command`, `cmd /c`), inline script
runners (`python -c`, `node -e`), pipes and redirection, and anything reaching outside the
checkout (publishing, registry credentials, `cargo install`, `npm i -g`). Every run is already
told not to reach for the first three. A fully-qualified program is fine — `C:\…\cargo.exe test`
classifies as `cargo`.

- `auto-grant budget exhausted after 4 grant(s)` means the task shells out too many *distinct*
  lines. Re-scope it, or forbid shell outright (see `verify.md`).
- A rule denied **twice** is a configuration problem, not something to retry.
- **Watch for safety-net bypasses after refusals.** A sub-agent refused by the permission gate may build a workaround that weakens a safety net. In one observed run, unable to set an environment variable due to shell restrictions, the agent added a backdoor flag file (`out/update_baselines.flag`) that silently overwrote regression test baselines. While presets like `code` now forbid this, always review diffs for such bypasses after any refusal.

## `guarded`

agy's gate is off, so **nothing dies at the gate any more** — none of the "forbid shell" shaping
applies, and asking for it wastes a round-trip. A PreToolUse guard refuses a small
destructive/outward-facing set instead: recursive or forced deletes, publishing, registry
credentials, PATH installs, `git push` / `reset --hard` / `clean -f` / `restore` / `checkout
<path>` / `stash`, machine reconfiguration, and writes outside the run's authorized roots.
**A refusal costs a sentence, not the run**: the sub-agent is told why and carries on, and you see
it in the answer.

Two things to know:

- **Don't have it write files through the shell.** `Set-Content` / `Out-File` / a here-string is
  inspected as if the file's *content* were commands, so a document that merely mentions
  `cargo publish` gets refused. Every run is told to use its own file tools; if you see this, that
  is the cause.
- **Widening is the operator's call, not yours.** If a refusal is genuinely wrong, say so and name
  the rule — `~/.agy-mcp/policy.toml`, `<workspace>/.agy/policy.toml`, or a preset's
  `policy_deny`/`policy_allow` are where it changes. Don't route around it.

## `trusted`

`--dangerously-skip-permissions` on every delegated spawn: nothing dies, and **nothing is
checked**. The sub-agent can do anything the user can. Ordinary care applies — scope the task,
keep it in one workspace, read the diff.

---

**Never add entries to `globalPermissionGrants.allow` in `~/.gemini/config/config.json`
yourself.** That list is the operator's own record of what they have manually approved; widening
it so one job succeeds changes their security posture without them agreeing to it. "Pre-grant what
it needs" means *ask them to*.
