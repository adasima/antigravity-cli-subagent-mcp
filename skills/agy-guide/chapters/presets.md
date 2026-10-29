# Presets, and keeping the reply short

## Presets cut *your* output tokens

`preset=` prepends a curated preamble so you don't hand-write boilerplate every call. Pair a
**terse goal** with a preset instead of a long prompt.

**Check for project-specific presets before writing one.** Beyond the built-ins, a repo can define
its own in `<workspace>/.agy/presets.toml`, and the user in `~/.agy-mcp/presets.toml`.
`agy_diag(action=status)` lists every available name. This matters: **a preset is where a
project's judgment already is**, so naming one is usually cheaper and better than re-deriving
those rules in your prompt. A name that doesn't exist is an error listing the real ones, so
guessing costs nothing but a round-trip.

**When you find yourself writing the same standing rules into a prompt twice, that content belongs
in a preset** — say so to the user rather than typing it again.

### The built-ins

- **`code`** — autonomous edit: smallest focused change, **no leftover scratch/temp files**,
  verify with build/test if obvious, reply with a short summary + changed files. Use this for
  delegated coding.
- **`research`** — read-only investigation; long findings written to a file; concise reply. Runs
  under the `agy-mcp-research` agent: read, search, web search and `write_file` only — **no shell**
  (it cannot run `git`, `cargo` or a script) and no edit tool. A job that needs a command run is a
  `report` job, not `research`. MCP tools stay reachable; pass `mcp="none"` to cut them. Its
  `write_file` can still reach your repo, so it carries `scope = ["{reports}"]`: a write
  anywhere else shows as `⚠ outside scope: …`.
- **`review`** — second opinion (use with `model="argon"`): reads, never edits; full review in a file
  under `~/.agy-mcp/reports/`; reply ≤8 lines of ≤200 chars (verdict, ≤3 risks with file:line, the one
  check to run first; each claim marked READ or GUESS). **Served on another model than asked → the run
  fails** (a ⚠ elsewhere).
- **`report`** — any task, but detail goes to a file and the reply is ≤5 lines.

`research` and `report` write their findings file to **`~/.agy-mcp/reports/`, not into your repo**
(the reply names the full path; files older than 30 days are pruned). That's deliberate: `wrote:`
is grounded in the workspace's git diff, so a report dropped in the repo would make every
read-only investigation look like a change. Nothing for you to pass — the bridge grants the
sub-agent access to that directory itself.

Presets are a shortcut, not a cage: you can still spell out everything in the prompt when you need
finer control. The preamble just stacks before it.

## A preset can make the bridge run one check for you

If a preset carries **`verify`** — a single build/test command; none of the built-ins do, this
repo's own `house` uses `cargo check --all-targets` — the bridge runs it in the workspace after a
successful run and folds the result into the footer: `✓ cargo check: ok`, or
`✗ cargo check: FAILED (exit 101) · error[E0432]: …`.

That is the bridge doing the post-delegation check `verify.md` asks for. **When you see a
`✗ verify` line the leftovers are already named** — resume that conversation with them instead of
running the build again. It is skipped when git proves the run changed nothing, so read-only work
never pays for it.

A preset can also name **`expect_files`** — the files a run must leave behind (e.g.
`["pkg/app.wasm"]`). One that is missing after the run shows as `✗ expected file missing: …`, so
a "Done" with no artifact no longer passes silently. Resume that conversation with it.

## Saying where a run may write (`scope`)

`agy_sub(scope=["src/core", "tests/core.rs"])` — or a preset's `scope` key — names the paths a
run may write. Any of its own writes outside them shows in the footer as
`⚠ outside scope: src/main.rs`. Nothing is blocked or reverted: when you see it, read that diff
and decide. Only the run's write receipts are judged; a file changed by its shell lands in
`also changed …` as before, unjudged. The argument replaces the preset's list, and
`~/.agy-mcp/reports/` is always allowed.

## Per-lane environment variables (`env`)

A preset can specify an **`env`** table of environment variables to inject into the sub-agent's
child process and the `verify` command:

```toml
[lane_a]
extends = "code"
preamble = "Focus on crate A."
env = { CARGO_TARGET_DIR = "{workspace}/target/lane_a" }
```

### When to use it
- **Parallel lanes on Windows**: When concurrent sub-agents build and execute Rust binaries in the
  same repository, Windows locks running executables under `target/`. Setting a distinct
  `CARGO_TARGET_DIR` per lane (e.g. `{workspace}/target/lane_a`) prevents build collisions without
  requiring ad-hoc command line flags or unsafe backdoor scripts.
- **Path substitutions**: Values support `{workspace}` and `{reports}` path substitutions, just
  like `preamble`.
- **Inheritance**: With `extends`, environment tables merge parent-first with the child winning per
  key ("nearest explicit answer wins").

### Safety and reserved names
To prevent breaking system hooks or hijacking toolchains, variable names must be valid identifiers
and cannot match (case-insensitively):
- `PATH`, `PATHEXT`, `COMSPEC`, `PSMODULEPATH`, `SYSTEMROOT`
- `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`, `CARGO_BUILD_RUSTC_WRAPPER`
- Any variable starting with `AGY_`
- Values cannot contain NUL or newline characters.

The bridge's internal control variables (`DELEGATED_ENV`, `AGY_CLI_DISABLE_AUTO_UPDATE`, `POLICY_ENV`,
`PATHEXT`) are always applied after preset variables, so bridge controls always win on any collision.


## Output size — keep your context lean

- The final answer is returned inline but **capped at ~16000 chars**. Nothing is lost to the cap:
  over it, the full answer goes to `<workspace>/.agy-answer-<id>.md` and you get a head plus that
  path. Read the file only if you need the rest.
- Better than relying on the cap: use **`preset=report`**, or **tell the sub-agent in the prompt
  to write detail to files and keep its reply short** — e.g. "write the full analysis to
  REPORT.md; reply with a 3-line summary". Created file paths come back in the footer.

## Honest caveat

What is measured is that **judgment written into the prompt** beats leaving it to model effort.
That the *same* judgment delivered through a preset performs identically is assumed, not measured.
So if a preset run comes back noticeably worse than the same rules typed inline, that is a
finding — say so to the user rather than quietly going back to long prompts.
