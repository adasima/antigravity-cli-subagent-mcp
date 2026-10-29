---
name: agy-guide
description: "How to delegate to the agy-mcp sub-agents (Gemini/Claude on subscription quota): which model, how to write the order, how to read the report. Use before writing an agy_sub prompt or when reading its result. Depth is in chapters/."
---

# Delegating to agy sub-agents

`agy_sub` runs an autonomous agent (file, shell, network) on the user's Google AI subscription
quota, with **zero metered billing**. Offload self-contained, tool-heavy work so it doesn't spend
*this* session's context.

## Which model

| line | for |
|---|---|
| `flash` | **the default**, code included. A strong tool user; weak on long input |
| `pro` | Gemini 4 Argon, own weekly bucket. **Let it read and think, let flash do.** Second opinion: `model="argon", preset="review"`, one run ≈ 1/10 of the bucket |
| `sonnet` / `opus` | the Claude bucket: prose, non-technical consultation, set-procedure code when GEMINI is out. No technical judgment. Prefer sonnet: opus hits that bucket's wall faster |

Escalate on evidence, not caution; a wasted flash run is cheap. `model="flash-low"` for rote work.
Other names, effort, resuming: `chapters/models.md`.

## Write the order

The sub-agent **executes well and judges badly**: whatever you leave out is what goes wrong.

1. **Goal.** What is true once done, and what it is for. Draw the big picture; it cannot infer
   the shape of the work from the code.
2. **Constraints.** Files it may touch, **what it must not touch, named explicitly**, conventions.
   A run scoped to one file once destroyed a sibling job's finished work because nothing said not to.
3. **Procedure.** Only an order that is actually fixed; otherwise leave the path to it.
4. **Acceptance.** A check that decides pass/fail without judgment (test or build command, expected
   output, a count that must match, files that must exist). Tell it to run the check, keep fixing
   until it passes, and quote the last output. The check, not a long order, brings back a finished
   product. **If you cannot write the check (looks right, design, root cause), it is not for agy**;
   read-only work (exploration, summaries, `review`) is the exception.

- `workspace` = the real project root, explicitly. Not a worktree or a copy (a cold `target/`).
- Inputs over ~30K chars go in a file: pass its folder via `add_dirs`, name the file in the
  prompt. Images and PDFs: `chapters/multimodal.md`.
- Look for a preset before writing a long prompt (`agy_diag(action=status)`); a repo's standing
  check belongs in its `verify` key: `chapters/presets.md`.
- Changing an output contract: budget an end-to-end run of the built artifact (`chapters/verify.md`).

## Read the result

The footer already answered these; don't redo them.

| signal | meaning |
|---|---|
| `wrote:` | this run's own write receipts, never the git diff. **Don't run `git status` to confirm** |
| `also changed … (no write receipt)` | a shell edit by this run, your own edit, or the named job's; not proof of a violation |
| `⚠ … may have punted` | no write receipts, backed by git |
| `✓/✗ task-N` · `✓/✗ <verify cmd>` | a background build's / a preset check's actual outcome |
| `QUOTA_EXHAUSTED:` · `AGY_UNAVAILABLE: … → model=X\|none` · `AGY_DOWN:` | retry or use `X`; `none` / `AGY_DOWN` = do it yourself |

Any other `⚠` or `⟨…⟩` line (line endings, stray artifacts, a repeated command, idle, a different
model served, uncollected results, dropped non-UTF-8 output): `chapters/verify.md`.

None of this says the *content* is correct. Read the diff for what matters: references it never
looked for, artifacts that were supposed to exist, contract-level bugs, "move verbatim" not honored.

## Never

- Poll `agy_poll` in a loop. Run `agy-mcp wait <job_id>` as a background Bash task: zero tokens,
  one round-trip. Once it runs, don't peek at its output or poll: do other work or end the turn.
- Add entries to `globalPermissionGrants.allow` yourself. If a refusal is wrong, say so and name the rule.
- Use agy's own `--continue`: it grabs the globally latest conversation, not yours.

## Chapters (`chapters/`, read one when you need it)

| file | when |
|---|---|
| `dispatch.md` | the call didn't come straight back: acks, `agy-mcp wait/poll/jobs/show`, timeouts, fanning out |
| `verify.md` | before a large refactor, a "success" you don't believe, a footer line not listed above |
| `permissions.md` | a run died at a permission gate, or a command was refused |
| `presets.md` | presets, the `verify` key, keeping the reply short |
| `models.md` | a heavier line, calling by name, effort, the model list rotating |
| `multimodal.md` | an image, a screenshot or a PDF |
| `quota.md` | a run failed on quota, or you want the headroom |
