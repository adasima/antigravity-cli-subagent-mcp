# Models — call any one by name

Every model agy lists can be called by name, through `model`:

- a line of `agy_diag(action=live, what=models)` — `gemini-4-argon`, `gpt-oss-120b`;
- a word that names exactly one line — `argon`, `sonnet`, `gpt-oss`;
- an exact slug, or agy's display label ("Gemini 4 Argon (Low)").

The newest version of the line is taken. End the name in `-low`/`-medium`/`-high` to fix the
effort; otherwise it gets the default below. An ambiguous name (`claude`, `gemini`) is refused with
the candidates, and an unknown one with its nearest siblings — both before anything is paid, after
the bridge has already re-listed the catalog. `model_label` takes the same names and wins when both
are given; it stays only for older calls.

## One line per model

What was observed while developing the bridge, one line each. **The evidence column says how
thick the observation is** — "wiring only" means the model was served as asked and nothing more:
not a verdict, and not to be read with the weight of a thick line. Older flash versions are left
out on purpose. Add a line when a model arrives.

The effort column is **the bridge's default**, then (in parentheses) what agy offers for that
model.

| model | effort | note | evidence |
|---|---|---|---|
| `gemini-3.8-flash` `[flash]` | medium (low·medium·high) | The default, code included. Never hedges: an ambiguity is silently resolved one way | thick |
| `gemini-4-argon` `[pro]` | high (low·medium·high) | Read-and-think line: let it plan, let flash do. Own weekly bucket. Quality and context size unmeasured | wiring only |
| `claude-sonnet-5-5` `[sonnet]` | medium (low·medium·high) | The Claude bucket's better use; where the quota-wall refusal points when GEMINI is out | wiring only |
| `claude-opus-5-5` `[opus]` | medium (low·medium·high) | Prose and non-technical consultation. Technical judgment unmeasured | wiring only |
| `gpt-oss-120b` | medium (not adjustable) | Call it as `gpt-oss`. Removal announced by agy | wiring only |

The CLAUDE AND GPT bucket is billed by token cost, and agy's own harness puts ~10–20k input tokens
into every call — even a bare "OK". Don't spend it on short errands.

## The aliases are nominal lines

`flash`, `pro`, `sonnet` and `opus` are resolved **before** any name, and each is a *role*, not a
version: the newest model in its slot. The `[flash]`-style tags in the listing show where each one
lands today. `pro` is "the top Gemini line" — today **Gemini 4 Argon**; when a codename replaces it,
the bridge maps it (`CODENAME_SLOTS`) and `pro` follows. Use an alias when the role is what you
mean, a name when the model is.

- **flash** is not cautious — it does not hedge, stop to ask, or flag an ambiguity it resolved —
  and it is thin on domain knowledge. Give it the moving parts and it gets them right; give it a
  vague direction and it misjudges the approach. **That is why the order carries the goal and the
  big picture** (see `SKILL.md`). What you forget to say is what goes wrong.
- **pro (Argon)**: don't hand it the whole job — its output costs too much. Let it **read and
  think**, then give the doing to flash. Effort: high for ordinary tasks, `argon-low` for document
  tidying; medium only for thinking-only work. **Price: one review (23 steps, ~29k ctx) took ~11% of the
  weekly ARGON bucket — about 9 runs a week at most.** Spend it on second opinions
  (`preset=review`), not routine work. The 3.1 Pro lessons are not carried over.
- **sonnet / opus** draw on the small CLAUDE AND GPT bucket the user also spends. For the same task
  flash is usually better; they are the way to keep working when GEMINI is out — sonnet first
  (opus hits that bucket's wall sooner). Opus 5.5 is for prose, non-technical/UX consultation, and
  fixed-procedure implementation; keep it off technical judgment until that is measured.

## Pinning a version or an effort

The newest version is almost always what you want. To pin an older one — reproducing a result, or
a newer version regressed on your task — name it: `gemini-3.7-flash` (its default effort) or a full
slug (`gemini-3.7-flash-high`). Version numbers rotate, so take names from the listing rather than
memory; a stale one is refused before dispatch, never silently run.

## effort

**There is no `effort` argument, by design.** The bridge applies a fixed default: **flash, sonnet,
opus and slot-less models → medium**, **pro → high**. agy's three Claude tiers are taken to match
Claude Code's low/medium/high (assumed from use; the mapping happens inside Google's backend).

**The one override in use is `model="flash-low"`** for judgment-free work — a mechanical edit, a
format conversion, a rename, a listing: every decision is in the prompt and the only thing left is
typing. It is much faster (~46 s vs ~95 s median) and as correct. Don't pick any other effort; if a run underperforms, the cheap next
step is a retry or a sharper prompt.

**Formatting rules in a prompt are not honored at any effort** (line width, wrapping). Enforce
them with a formatter.

## Trust the served-model receipt over your assumptions

agy's picker sometimes doesn't honor a slug its own model list advertises and **silently serves a
different model**. The bridge catches it — by slot, or by family for a model without one — and
prepends `⚠ requested "X" but agy served "Y"`. **When you see it, the run did NOT use the model you
asked for.** Name a different model and retry. Which combos are affected rotates server-side, so
don't memorize any of them: the warning is the detector. `agy-mcp stats` turns it into a standing
rotation monitor, per requested model.

## Resuming

A bare resume (no `model`) keeps the conversation on its own model — a slot model re-resolved to
the current slug, a slot-less one (GPT-OSS) by its family. Naming a model on a resume switches the
conversation to it.

## When the model list rotates

Nothing for you to do. If a model is renamed, the bridge re-lists headlessly (`agy models`, zero
quota) and retries the resolution within the same call; the cache also self-refreshes at server
start. An error only reaches you if the name is missing from a *freshly fetched* list — and it
says so, with what is there.

The default model of a fresh run is `flash` unless the operator set `AGY_DEFAULT_MODEL` at setup;
if neither resolves, the run is refused rather than guessed.

---

Dated arm-by-arm numbers, and the runs behind these judgements:
`G:\workshopG\gemini-subagent\docs\model-notes\current.md` and
`G:\workshopG\gemini-subagent\docs\ordering-conventions.md`.
