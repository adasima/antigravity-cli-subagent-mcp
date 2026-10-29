# Verify — the failure modes the bridge cannot see

The bridge grounds *whether* something happened (see the signal table in `SKILL.md`). It says
nothing about whether the content is **right**. Everything below is a way a run comes back looking
successful while being wrong. All of it is observed behaviour, not caution.

## Re-check the references yourself

Even given a precise delete/edit spec, the sub-agent misses non-obvious references: a sibling
module that used a removed symbol, a now-dangling call, type widening from a conditional spread.

The `code` preset tells it to grep all references and run the build **in the foreground to
completion**, and forbids replying "still running" — but **agy frequently ignores this**: it runs
`cargo build` as its own background task, says "building…", and ends the turn. The bridge recovers
that build's real outcome for you (`✓/✗ task-N`). What it cannot recover is a reference the
sub-agent never looked for.

So after the run, run the project's own check (`cargo check --workspace`, `npm run check`)
yourself and fix or re-delegate the leftovers. **A preset with a `verify` key does exactly this
for you** — see `presets.md`.

## A clean "Done" is a claim, not a result

For a `wasm-pack` / `cargo` / `npm` step, check that **the specific artifacts the task was
supposed to produce** actually exist on disk — the `.wasm`/`.js`/`.d.ts`, the binary, the new
route. `git diff` alone won't catch this: a CLI argument mistake can fail the build, write zero
output, leave only stray 0-byte scratch logs, and still report full success.

**When the success criterion is "these artifacts exist", a second independent pass is worth the
round-trip.** A fresh `agy_sub` — no `conversation_id`, told to check disk only — cannot inherit
the first run's false assumption. A same-session re-check, primed by the same narrative, can.

## A green build is not a result either — run the thing

Two real defects have come back *compiling cleanly*, invisible to both `git diff` and
`cargo build`:

- the new code returned a summary and silently dropped the full payload, so the data became
  unreachable;
- the envelope it built was wrapped a second time by an outer layer, folding the summary away
  entirely — the caller received a preview of a preview.

Both are **contract-level**: the types line up, so the compiler has nothing to say. They surfaced
only when the built binary was driven end to end with a realistic fixture, and a third defect was
found the same way *after* the first two were fixed.

**So when you delegate a change to an output contract** — a tool's response shape, a serializer,
an API envelope, a formatter — budget an end-to-end probe of the built artifact and treat "the
build passes" as the start of verification, not the end. **Put non-ASCII in the fixture**: the
same review turned up a `&s[..100]` byte slice that panics on any multibyte string.

## Where the repo has an invariant test, add a witness line

Delegated changes have violated a containment invariant and **come back with the test suite
green** — the corpus simply had no line that exercised the violation. Adding one witness input and
re-running found it in seconds, where reading the patterns did not.

**Green means "no counterexample in that corpus", not "correct."**

## "Move this code verbatim" is not honored — audit it, don't read the diff

Told repeatedly to move code unchanged, a sub-agent still dropped two items, retyped a function's
return type, dropped a dependency-injection parameter, dropped two call sites, and **replaced a
cross-drive workaround with `std::env::current_dir()`** — inventing a plausible-looking body. All
of it compiled and all tests passed, because it had rewritten the affected tests too. A `git diff`
on a 5000-line move is unreadable.

For any large mechanical refactor:

1. Keep a **read-only copy of the pre-change file inside the workspace** so the sub-agent can diff
   against it — it cannot run git.
2. Check mechanically: extracted item names, full signatures, test names, comment-line sets, and
   per-identifier occurrence counts, old vs new.
3. **`never used` warnings are your dropped-call-site detector** — read them, don't silence them.

Full recipe: `G:\workshopG\gemini-subagent\docs\design-notes.md`.

## Self-reports on line endings, headings, and visuals are not evidence

A sub-agent's self-report about line endings, headings, or "fully resolved" visual results is never evidence — open the file or inspect the image yourself. Observed runs have claimed "CRLF maintained" while writing LF, claimed removing non-existent headings, or reported "shoulder clump completely gone" while the rendered PNG showed the defect unchanged (all with green builds and tests).

Judge line endings with `git ls-files --eol <path>` (inspecting the `i/` and `w/` columns). Never count carriage returns with Git Bash `grep -c $'\r$'`: it strips the CR and falsely reports every file as CRLF.

## Reading the new footer segments (2026-09-15)

`wrote:` is now the run's own write receipts only — never the git diff (before this, a run
sharing a tree with sibling jobs or the supervisor's own edits could claim credit for their
changes). If git saw more in the tree than the receipts account for, it shows up as its own
segment, `also changed during run (no write receipt): <paths>`. That segment does not by itself
mean something is wrong: it can be this run's own edit made through a shell command instead of
agy's write tool, which the transcript never receipts. When another job (or another agy-mcp
process's job) was running in the same workspace during this run's window, its id is named beside
it — `other jobs ran in this workspace meanwhile: jN, …` — which is the case to treat as
genuinely unattributable. **No overlap named does not make it this run's**: your own edits during
the run (and any other tool's) are not jobs, so they are never named. Before reading a path there as
a constraint violation, check whether you touched it yourself while the run was in flight.

The punt nudge (`⚠ … may have punted`) now fires only when there are **no write receipts at
all**, backed by git: either a clean tree, or a non-empty diff that falls entirely inside another
job's overlapping run window (named in the message). A no-receipts run whose diff is
unattributable to any other job is deliberately **not** flagged — that combination reads as
"edited via shell", not a punt.

Three more `⚠` segments flag a run that went sideways without failing: `⚠ eol changed` (it
flipped line endings), `⚠ stray artifacts` (it left `*.old` / `*.tmp` behind), and
`⚠ ran "…" N times` (a sweep that lost its way). And `⟨N line(s) dropped: non-UTF-8 console
output⟩` is a red flag: a child process's output was destroyed before it reached the bridge, yet
the sub-agent saw the wreckage and will quote the "error" back fluently from memory. Treat any
claim about those lines as unverified.

`QUOTA_EXHAUSTED: …` in a failure means agy hit repeated 429s and the bridge stopped the run
rather than let it idle for minutes — see `G:\workshopG\gemini-subagent\skills\agy-guide\chapters\quota.md`
for what to do next.

## Cheaper than verifying: shape the dispatch

**For a multi-round refactor, forbid shell from the start and run the build yourself.** Permission
grants are exact strings and the recovery budget is 4, so a sub-agent that tries `cargo check`,
`cargo test --all-targets`, `cargo test --bin …`, `cargo test --test …` has burned it before
reaching `cargo test --lib`. Saying *"do NOT run any shell command; file read/write only; I run
the build"* and pasting back a deduplicated error digest took a 99-error state to 14 in one round.
The same rule kills the wrapped-command death — a run that finished all 11 files and then died on
`powershell -Command "Remove-Item …"`, which can never be auto-granted.

**Hand over hundreds of small mechanical edits as a worklist file** (`<original line no>: <item>`),
passed via `add_dirs`. It doubles as the progress marker when a run stops early.

## When to stop delegating

If escalation still doesn't stabilize a **precision-critical core implementation**, stop
re-delegating and **write it yourself** — that is faster than another retry round. And before
delegating work against an unfamiliar API, check the real signatures in source first: a
sub-agent's recollection of a less-common library can be stale or invented.
