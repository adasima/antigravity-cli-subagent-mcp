# Quota

## Two buckets

- **GEMINI** (flash, pro) — a weekly and a 5h window. **Which one binds depends on the week:** a
  flash run costs roughly 1% of the week, so a week of heavy fan-out (a dozen runs a day) empties
  the weekly bucket first, and then nothing runs for days whatever the 5h window says. Read the
  burn line's `~N runs left this week`, not a rule of thumb.
- **CLAUDE AND GPT** (sonnet, opus) — separate, with its own weekly/5h limits, and it can exhaust
  independently. It is the smaller bucket.

Nothing is shared between them: a wall on one says nothing about the other, and when GEMINI is
out the Claude bucket is often untouched — still zero-billed (see `models.md` for what opus is
good for).

## The state is in front of you before the first call

The server's `instructions` (read once, at session start) carry a line like
`agy quota (cached 38m ago): GEMINI ✗ weekly (resets in 1d9h) | CLAUDE AND GPT 5h 100% / weekly 13%`.
It is the cache, dated, and never updated mid-session — a refusal carries later news.

## Don't pre-check before every call

`agy_diag(action=live, what=quota)` costs no agy quota and answers in a second or two, but every
call is still a round-trip in your context, and the bridge already refuses an exhausted group up
front from its cache. Blanket-checking buys nothing on routine asks.

The cheap check is the CLI, which reads local files only — no agy launch, no quota, no tokens:

```
agy-mcp quota      # newest reading per group, with its age, plus burn
agy-mcp stats      # per-model profiles from the run-log, and a mismatch-rate block
```

Reach for a live read in essentially one situation: **right after a run fails**, when you want
the exact remaining % and reset time rather than retrying blind.

## A run stops itself after 3 consecutive 429s

agy retries an exhausted quota with backoff and then goes silent without exiting — left alone,
that used to only surface at the idle timeout, 10+ minutes later, indistinguishable from a slow
run. The bridge now counts consecutive 429s at the transcript's tail and stops the run itself
once they hit 3, failing fast with a message starting `QUOTA_EXHAUSTED:`. That message names the
**other** quota group's model (`opus` for a Gemini wall, `flash` for a Claude wall) when that
group's own cached reading is not itself exhausted — retry the same model later, or resume the
same `conversation_id` with `model=<the named one>` right away.

The count covers **only the steps this run wrote**. A resume of a conversation whose last turn
hit the wall starts clean, so the hand-off above is not stopped by the earlier turn's 429s.

## The reading keeps itself current

Two things feed it without you:

- a run that dies on a **429 records that as a 0% reading** for its group — the receipt is
  authoritative, a live read would only confirm it (this includes the bridge's own quota-cutoff
  stop above, not just agy's own eventual failure);
- after any run finishes, the server re-reads `/usage` if the newest reading is over 30 min old.
  It only takes a free slot, never waits for one, so it never delays a dispatch.

## The guards this buys

- **`agy_sub` auto-refuses a known-exhausted model.** If the newest reading saw the target's group
  at ≤1% on **either** window, before its refresh time (or <4h old when none was logged), the
  dispatch is refused up front — a local file read, zero latency — instead of paying a 429 you
  could have predicted. The refusal's first line is fixed:
  `AGY_UNAVAILABLE: GEMINI weekly (resets in 1d9h) → model=claude-sonnet-5-5` (a model name from
  the catalog — pass it as `model` as-is). The window tells you hours
  from days; `→ none` means the other group is out too; `429` in the window slot means the only
  evidence is a 429 receipt, which can't say which window. `force=true` bypasses it; no reading
  at all means no guard, so it never blocks blind.
- **`AGY_DOWN: …`** — agy itself failed the zero-quota probe (`agy models`): a missing or broken
  install. Nothing can run; do the work yourself and check `agy_diag(action=status)`.
- **A failure that looks like something else gets a quota hint when it fits.** If a run fails with
  an empty/contentless transcript and the cached snapshot for its group is exhausted by the same
  rule, the error is annotated `probable cause: quota — …`. That is a correlation with the last
  reading, not a receipt from that run: treat it as a lead, not certainty. A `QUOTA_EXHAUSTED:`
  failure likewise gains a `window:` line when a live reading names the window.
- A run that fails right after a switch to sonnet/opus is plausibly the Claude bucket. The bridge
  says so explicitly if agy logged a 429; if not, a live read gives the exact reset time. (The
  IDE shows a "Baseline model quota reached … refreshes on <time>" banner that headless mode does
  not surface.)

## Read the burn line, or read nothing

The burn estimate aggregates over a 24h window and **drops intervals where the rolling 5h window
refilled**. Below a minimum sample it prints `not enough measured runs (n=…, need 5)` rather than
a number it cannot support.

So: **a printed rate is worth reading; silence or "not enough" means unknown, not zero.**

## Inspecting a run (no quota)

- `agy_diag(action=trace, conversation_id=…)` — compact per-step trace; `full=true` adds thinking.
- `agy_diag(action=sessions)` — recent conversations with ids and sizes.
- `agy_diag(action=status)` / `agy_diag(action=mcp)` — health, resolved workspace, permission
  posture, available preset names.
- `agy-mcp stats` / `agy-mcp quota` — CLI, deliberately off the always-loaded schema.

The report footer (`↳ N steps · wrote: … · ~Nk ctx`) already gives steps, artifact paths and the
resumed ctx estimate without a follow-up call. Note `~Nk ctx` measures *persisted* transcript size
and under-counts when prompts are huge — agy truncates large inputs on persist.
