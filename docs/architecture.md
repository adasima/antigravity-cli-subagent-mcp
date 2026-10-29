# Architecture and tool behavior

Moved out of `AGENTS.md` on 2026-09-29 so the always-loaded file stays small. Read the section of
the module or tool you are about to change before editing it. Rules that must never regress stay
in `AGENTS.md` (`Critical invariants`).

## Test map


**`cargo test`** covers the pure logic — keep changes there covered. By module (under `src/`):

- `main.rs` — `plan_dispatch`, `ctx_limit_for`, `ctx_over_note`;
  `strip_defaults`/`normalize_opt_arg`; the `wrote:` / punt-nudge attribution split
  `punt_evidence`/`git_only_paths`/`norm_path_for_match`; `punt_nudge_text`;
  `rel_under_workspace`/`fmt_paths_capped_with_notes`; `looks_stray`/`stray_artifacts`; `outside_scope`; `missing_expected_files`;
  `resolve_gemini_from`/`resolve_claude_from` (newest version incl. a bare major, effort ladder);
  `resume_keep_from` (a bare resume keeps a slot-less model by `family_key`);
  `resolve_by_name`/`name_effort` (exact slug → family → the one line every word names; newest
  version; a named effort is exact; two lines is an error listing them).
  `no_default_model_text`/`default_model_line` (`AGY_DEFAULT_MODEL` → `flash` → refuse, never a guess).
- `agy/run.rs` — `parse_agy_error` (last `AGY_ERROR:` line → `k=v`; exit 3 keeps the stdout fragment,
  an answer + receipt gets a ⚠ note), `stop_message`, `should_stop_quota`, `stop_forces_failure` (QuotaWall with a
  partial answer → Err), `repeated_command_warning`/`normalize_command`, `quota_wall_stop` (a
  resume ignores the previous turn's trailing 429s), `stop_snapshot`/`shell_names` (the one-line state at an idle/cap stop), `PipeReader` (a pipe a leftover child holds
  open stops being read after `PIPE_CLOSE_GRACE_S`, keeping the bytes already read).
- `agy/transcript.rs` — `steps_after`/`read_full_since` (a resume reads only its own steps),
  `parse_full` artifacts from agy 1.4.x `edit_file`/`write_file` receipts,
  `clean_user_prompt` (a session head skips the workspace directive),
  `analyze_task_log_content`, `find_test_summary` (cargo suites summed), `strip_broken_encoding`,
  `first_meaningful_line` (GENERIC receipt vs. view_file content),
  `median_step_interval_s`/`recent_step_timestamps`/`last_step_line` (poll line step pace).
- `agy/quota.rs` — `parse_usage_json`/`iso8601_utc_to_unix`/`parse_credits_json`, and the 429 cutoff's quota group and
  alternative-group hint `resolve_quota_group`/`quota_group_of` (GPT-OSS → Claude bucket, an argon label → the optional `ARGON_GROUP`, never `GEMINI`; `parse_usage_json` reads `disabled` buckets; `unguarded_groups` names a `/usage` group with no `KNOWN_GROUPS` entry; `slot_of` + `CODENAME_SLOTS` (argon → `Pro`); the argon group is never "missing" and never `?` in `status_line`)/
  `quota_alternative`/`other_quota_group` (a concrete catalog name: newest Sonnet / newest Flash line); the
  pre-flight guard `CachedPct::is_exhausted`/`exhausted_window`/`quota_unavailable` (both
  windows, `AGY_UNAVAILABLE:` head), `status_line` (the `instructions` line),
  `burn_estimate`/`format_burn`/`weekly_runs_left`.
- `agy/git.rs` — `looks_binary`/`count_lines`/`line_count_note`/`dominant_eol`/`wrote_path_note`
  (`wrote:` line counts, EOL detection).
- `agy/jobs.rs` — `overlapping_jobs` (attribution split), `running_in_workspace`/
  `shared_tree_note` (the dispatch-time shared-tree line), `format_lost_job_text`
  (+ `written_before_stop_line`),
  `waiter_pid_is_live` (a `.waiter` counts only while its `agy-mcp wait` lives).
- `agy/git.rs` — `parse_check_attr_text` (only `text` set/auto paths count as normalized).
- `agy/run.rs` — `stopped_without_answer` (a no-answer idle/hard-cap stop becomes a cut-off outcome);
  `image_paths_in_prompt` / `image_looks_complete` / `broken_image_note`.
- `cli.rs` — `wait_run_ceiling`/`job_past_ceiling`.
- `agy/permissions.rs` — `auto_grant_rule`, `merge_trusted_workspace`, `parse_permission_mode`,
  `verification_command_grants` (its patterns are matched against an RE2 engine, so a widened
  family or a lost anchor fails here instead of on a live run).
- `agy/models.rs` — `slot_mismatch` (slot-less sides compared by `family_key`, vendor prefix only), `slot_of` (lite tier → no slot), `model_version` (dotted,
  dash-split Claude, bare major `gemini-4-…`), `slug_from_models_line` (slot-less rows kept),
  `format_models` (one line per family, alias tags), `is_invalid_model_selection`, `wire_model`, `family_key`
  (slug and display label → one family name; `(Thinking)` kept).
- `agy/jobobj.rs` (Windows only) — a real powershell + grandchild: dropping the job kills the grandchild, a disarmed job kills nothing.
- `agy/run.rs` (bg results) — `dedupe_task_results`/`condensed_task_result_lines` (last result per command; all-ok → one line); `agy/transcript.rs` `launch_commands` (task id → its `run_command`).
- `agy/customizations.rs` — `frontmatter_problem`.
- `agy/agents.rs` — `builtin_agent`, `ensure_builtin_agent_in` (writes a missing or differing shipped definition, leaves an identical one, never touches an unknown name).
- `main.rs` (workspace) — `workspace_missing_message` (refuse a missing workspace before spawn).
- `main.rs` (review) — `strict_model_preset`/`strict_model_message` (preset `review` fails on a served-slot mismatch).
- `presets.rs` — `resolve_preset`/`merge_presets`,
  `validate_preset_env`/`is_reserved_env_name`/`substituted_env`, the `agent` key (first-wins down `extends`, bad names refused).
- `hookguard.rs` — the hook guard's `decide`/`split_for_inspection`/`merge_hook_config`.
- `nest.rs` — what `spawned_env` hands down reads back as marked, and so does an older depth number.

A test also parses the repo's own `.agy/presets.toml` at build time, so a typo in it fails
`cargo test` instead of a delegated run.

Everything else needs **real MCP stdio**: Python harnesses in `scratch/` (gitignored;
`smoke.py` and `ctx_watchdog_probe.py` are stale, `split_verify.py` — post-refactor sanity sweep,
`presets_probe.py` — layered/failing preset resolution (answers `roots/list` like a real client,
which is the only way a project-local presets file becomes visible), `claude_test.py` / `live_test.py`) and the agy-behavior probes in `scripts/`
(`resume_model_switch_probe.py`, `fresh_conv_id_probe.py`, `concurrent_resume_probe.py`,
`short_timeout_probe.py`, `mcp_swap_timing_probe.py` — when agy reads `mcp_config.json`, with a
`--sweep` mode for the boundary; `mcp_scope_parallel_probe.py` — a scoped and an inheriting run
dispatched together each see their own scope; `agy_1128_contract_probe.py` — drives `agy -p`
directly, with no bridge in the way, to record what agy actually emits: the `--output-format json`
envelope, the soft-deny stderr markers, and `--print-timeout` expiry). Re-run the contract probe
after an agy upgrade; its `deny_*` probes need the machine temporarily on the `gate` posture. The
`/usage` JSON shape is pinned by the `USAGE_JSON` fixture in `quota.rs`; re-capture it from
PowerShell (`agy -p /usage --output-format json`) when agy changes it.

MCP stdio is newline-delimited JSON-RPC; keep the child's stdin **open** until responses arrive
(EOF makes rmcp cancel in-flight calls). Drive it from Python or a UTF-8/LF file via `cmd < file`
— PowerShell's `StandardInput` mangles bytes.

**Never infer success from a filtered build log.** Piping for readability
(`cargo build --release 2>&1 | tail -60`) reports the *pipe's* exit code, not the build's, so a
failed build can read green once its error scrolls past the window. When a build or test result
gates a "done" claim, capture the real code (`; echo "EXIT=${PIPESTATUS[0]}"`, or run it unpiped
first).

Token baselines to compare against before optimizing: see design-notes.

## Architecture

- **`src/main.rs`** — the rmcp server: the 3 `#[tool]` methods, arg structs, model-slug consts,
  the slot resolvers (`resolve_gemini`/`resolve_claude`/`resolve_sub`), `with_footer`, and the
  job registry (`dispatch` + `spawn_job` + `job_ack` + `agy_poll`).
  **The dispatch decision is the pure `plan_dispatch`** (`switch` label + `ResumeFacts` + `force`
  → `DispatchPlan { model, continue_id, model_disp, ctx_warn, ctx_limit }`); the quota refusal
  is the pure `agy::quota_unavailable`. `agy_sub` only gathers the facts — slot resolution (async) and the disk reads
  `session_ctx_tokens`/`session_model`/`resume_keep_model` — so resume/switch/keep-model/
  flash-guard/ctx-warn stay covered by `cargo test`, not only by a live MCP call. `keep_model` is
  deliberately resolved **only for a bare resume** (a switch ignores it).
  **Every run is a detached job; there is no sync/async choice.** `agy_sub` ends in
  **`dispatch`**: `spawn_job` registers the run (detached `tokio::spawn`, not tied to the request
  future, plus a `oneshot`) and `dispatch` **awaits the result**. Cancellation can't kill a run —
  it finishes and stays recoverable via `agy_poll`; by design an abandoned run still spends quota.
  **No 25s ack by default**: Claude Code backgrounds a call that outlives
  `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS` (default 2 min) itself, an early return prevents that, and
  how long a session may block is the client's setting, which only the user knows.
  **Where a surface bounds a single request, an ack keeps the bridge inside the bound — a
  per-client default, not a knob** (pure `client_inline_wait`, keyed on the name `initialize`
  reported): **`local-agent-mode-*` (Claude Desktop) → ack at 45s**, since it cuts a call at
  **~60s** whatever the documented levers say; everything else → wait for the result.
  `AGY_INLINE_WAIT=<secs>` overrides; `AGY_INLINE_WAIT=0` means "never ack". **Add a measured
  client to that function rather than telling users to set a variable** — they have no better
  information than the measurement. Per the docs stdio has **no per-request timer** (wall clock
  `MCP_TOOL_TIMEOUT`, ~28h, plus a 30-minute idle timeout a response or progress notification
  resets); this surface does not behave that way.
  **Progress while the call is open** (`RunOpts::progress`): the heartbeat pings every
  `PROGRESS_TICK_S` (15s) with the pure `progress_line`, which `main.rs` turns into
  `notifications/progress` for the token in the request `_meta` (no token = no pings, normal). A
  plain `Fn(u32, String)` callback, **not** an rmcp type — `src/agy/` carries no protocol crate.
  **Opt-in (`AGY_PROGRESS=1`), default off**: they make `local-agent-mode-agy` abandon the call at
  ~60s with `Request timed out`. **Keep the code** — a client that honours them needs the pings
  against the 30-minute idle abort and a mute blocking session. **Do not describe
  auto-backgrounding as the mechanism in play on this surface** (it did not fire even with
  `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=60000`); the call simply completes. A cut call loses the
  request, never the run: `agy-mcp wait` returns the full result.
  Concurrency: a **`Semaphore`** (`AGY_MAX_CONCURRENT`, default 10) bounds parallel runs; a short
  **`spawn_gate`** mutex serializes only fresh-run conv-dir detection; a **`swap_gate`** `RwLock`
  bounds the MCP-config swap to the spawn window (invariant #6); a fast `Arc<Mutex<State>>`
  (never held across a run) holds `last_conv_id` + the `jobs` map so diagnostics stay responsive.
- **`src/agy/`** — pure logic, no rmcp: spawn `agy -p`, read the answer, the model cache + slot
  classification, the `/usage` JSON parser, and the transcript readers (`read_full` →
  answer/served/artifacts/steps/ctx-tokens, `read_trace`, `list_sessions`,
  `session_ctx_tokens`/`session_model`).
  **`mod.rs` is declarations + `pub(crate) use <submodule>::*;` and nothing else** — call sites
  stay `agy::X`, and a glob re-export can't try to re-export a private helper (`E0364`).
  Cross-module references use absolute `crate::agy::<module>::<item>` paths, including inside
  `#[cfg(test)] mod tests` where `super` means the submodule, not `agy`. Visibility is per item in
  its own module: private unless something outside it refers to the item.
  - `run.rs` — `run_agy`/`run_agy_once`, `RunOpts`/`AgyOutcome`, `resolve_outcome`, liveness,
    `wire_model`, the quota-retry cutoff (`should_stop_quota`) · `transcript.rs` — `parse_full`,
    `read_full`/`read_trace`, sessions, task logs, `trailing_quota_retries` · `models.rs` — cache,
    `Slot`, resolution, `agy models` · `quota.rs` — run-log/stats, burn rate,
    quota log + `parse_usage_json`, `resolve_quota_group`, `quota_alternative` ·
    `permissions.rs` — grants, `auto_grant_rule`, trust merge · `jobs.rs` — `JobRecord` + the disk
    mirror, `overlapping_jobs` · `paths.rs` — dirs, conv-dir resolution, instance registry ·
    `mcp.rs` — config swap + `McpGuard` · `git.rs` — porcelain snapshot/diff (fingerprints every
    listed path, tracked and untracked) · `status.rs` · `customizations.rs` — skill +
    workspace-MCP discovery and the frontmatter check.
  **Edit receipts and background-task launches are `GENERIC` transcript entries** (agy 1.2.x;
  `CODE_ACTION`/`RUN_COMMAND` parsing kept for older transcripts, both feeding the same artifact
  list and `pending_tasks`/`task_results`). `GENERIC` is also a `view_file` result, so an entry
  counts **only when its first meaningful line** (`first_meaningful_line`, skipping agy's
  `Created At:`/`Completed At:`/`Using prompt:` headers) starts with the marker phrase — matching
  anywhere would turn viewed file text into fake write receipts or fake pending tasks. Task
  **completions** stay `SYSTEM_MESSAGE`-only (no `GENERIC` completion observed). Miss this and a
  run has no artifacts and no pending tasks: `wrote:` goes git-only, `drain_pending_tasks` never
  holds a job.
  agy 1.4.x adds two `GENERIC` receipts: `edit_file`'s `Made N replacement(s) in <path> …`
  (`replacement_receipt_path`) and `write_file`'s bare `Created file.`, whose path comes from the
  preceding step's `write_file` call (`write_call_path`, queued in `pending_writes`, cleared by
  each new tool-call step).
  **The wait-only auto-resume judges `AgyOutcome::bare_answer`, not the folded `text`**:
  `append_task_results` always pushes the answer past `is_wait_only`'s length cap, so a turn ending
  on "waiting for the build" with a finished task attached would never resume. `bare_answer` is
  agy's answer before that fold and before `agy_truncation_note`/interruption prefixes.
  **A cut-off run gets one summary resume.** Ending cut off with no final report (agy's own
  `--print-timeout`, or a bridge idle/hard-cap stop — `is_cut_off`) triggers one same-conversation
  resume asking for what it changed, verified and left unfinished, prefixed
  `· auto-resumed for a summary after a cut-off run` (mirrors the wait-only resume). **Never for
  QuotaWall** (an Err earlier), **never more than once**; a failed resume leaves the original
  outcome untouched. Inherits the preset's `env`.
  **Heartbeat (liveness) timeout, not a fixed total wall-clock**: `run_agy` (`run.rs`) `select!`s
  `child.wait()` against a `HEARTBEAT_TICK_S` poll and `start_kill`s agy only after
  `idle_timeout_s` with no liveness (`conv_liveness` = newest mtime across the brain conv subtree +
  the SQLite store), bounded by `timeout_s` as a generous hard ceiling (also agy's
  `--print-timeout`). Defaults: `idle_timeout_s` 180 (`AGY_IDLE_TIMEOUT`), `timeout_s` 3600
  (`AGY_TIMEOUT`; not an MCP arg — nobody blocks on it, so it's sized for the longest run worth
  finishing). Liveness keys on the **SQLite WAL**, not `transcript.jsonl` (why: design-notes). Both
  caps use `wall_since` (**wall clock**) — `Instant` can stop across a suspend and leave a dead run
  un-reaped. With a `job_id` set, each tick mirrors liveness into the job record.
  **Third stop reason: `StopReason::QuotaWall`** at `AGY_QUOTA_RETRY_CUTOFF` consecutive 429
  retries (default 3; `AGY_QUOTA_RETRY_CUTOFF=0` disables), because agy retries with backoff and
  then goes silent without exiting. Each retry is an `ERROR_MESSAGE` entry;
  `Transcript::trailing_quota_retries` counts the **consecutive** ones at the tail (any other
  entry, a transient 503 included, resets it), checked via `should_stop_quota` on each tick where
  liveness advances. The failure starts `QUOTA_EXHAUSTED:` and **still contains
  `RESOURCE_EXHAUSTED (code 429)`** so `record_quota_wall` learns the reading, and names the other
  group's model when that group's cached reading is not itself exhausted (`quota_alternative` /
  `other_quota_group` — `opus` for a Gemini wall, `flash` for a Claude wall). `record_quota_wall`
  falls back to the conversation's served model (`resolve_quota_group`, via `read_conv_model`):
  a **bare resume** passes no `--model` and would otherwise teach the guard nothing.
  **QuotaWall with a partial answer is a failure**: `stop_forces_failure` diverts it to Err with the
  `QUOTA_EXHAUSTED:` wording, partial answer as context, through `record_quota_wall`, the
  alternative group hint and the recovery hint. Idle/HardCap stops with a partial answer stay Ok.
  **agy's own `--print-timeout` exits 0 with partial output** and
  `[agy] print timeout after …; returning partial output` on stderr — indistinguishable from a
  finished run, so the pure `agy_truncation_note` reads that marker and prefixes the answer (the
  counterpart of `StopReason::partial_note`). Rare (the 180s idle cap fires long before the 3600s
  ceiling), which is why it would otherwise go unnoticed.
  **A headless failure ends with `AGY_ERROR: {...}` on stderr** (agy 1.2.6+); `parse_agy_error` leads
  the exit text with it as `k=v` (no key name is assumed — only `short_error` is confirmed, no real
  sample yet) and shrinks the raw stderr tail to 300 chars. stdout said before the error is kept as
  `partial answer:` (exit 3), and an answer followed by a receipt carries a ⚠ note without `cut_off`.
  **agy can cut its own stream mid-turn**: exit 1, `subscriber fell behind updates` on stderr, no
  final answer. The pure `agy_stream_interrupt_note` names it on the error and on a partial answer;
  `JobRecord::recovery_hint_after` adds the fresh-run way out to the resume hint. `read_full`
  returns **`NoAnswerError { partial, msg }`** on an empty answer, which `resolve_outcome`
  downcasts so `progress_note` and the run log keep the steps — do not flatten that error back into
  a string. The trust/OAuth text is reserved for a transcript with no tool calls and no artifacts.
  **Delegated runs set `AGY_CLI_DISABLE_AUTO_UPDATE=true`** — `true`, not `1`, which agy ignores;
  print mode's updater can replace the binary under a running job. Interactive agy still updates.
  **agy's stdout/stderr are read by `PipeReader`, and joined with a `PIPE_CLOSE_GRACE_S` bound
  after agy exits — never unbounded.** A child agy started can inherit the pipe and hold EOF open
  past agy's own exit, when the heartbeat loop has already ended and no idle cap can fire.
  **Backgrounded-build capture** — agy often shells `cargo build` as its own background task. Up to
  1.2.8 it ended the print turn while the task ran on; since 1.2.9 headless waits for it, up to
  `--print-timeout` with a 30-minute cap, so the drain below only matters past that cap (the
  `SUB_TIMEOUT_S` ceiling is 60 minutes) and is a no-op first check otherwise. `parse_full` pairs task launches (`GENERIC` per
  `first_meaningful_line` above, or `RUN_COMMAND`) with their completion; `drain_pending_tasks`
  holds the job until they land, then `append_task_results` folds
  `✓/✗ task-N: ok|FAILED · <tail>` into the answer. Ground-truth fallback: `read_task_log_result`
  (`.system_generated/tasks/<task-id>.log`), decided by the pure `analyze_task_log_content`. **Its
  ordering is load-bearing — do not reorder it** (test summary → `error[` → `` Finished ` `` →
  None); the bug it prevents is in design-notes.
  **The receipt path has the same rule, in `bg_finish_ok`**: agy words a completion more than one
  way (`The command completed successfully.` / `The command exited with code 0.`), so never decide
  on the phrase alone — failing test count beats a zero exit, the exit code decides when known,
  the phrase is the last resort. Keep `parse_exit_code`'s key list ahead of it: `exited with code`
  is agy's wording and is **not** matched by `exit code`.
- **`src/hookguard.rs`** — the **opt-in PreToolUse guard**, the one piece of policy that runs
  *inside* agy (rationale and measurements: design-notes). `toolPermission=always-proceed` removes
  the headless soft-deny (which kills a turn with no answer); this hook puts a **denylist** back. A
  hook `deny` is honored, outranks the allowlist, and the model **keeps going** and reports the
  refusal — a permission decision costs a sentence, not the run. **A hook can only subtract**:
  `decision:"allow"` does not lift the gate (`permissionOverrides` included), so "keep
  `request-review` and wave the safe calls through" is not buildable. `decide` splits the line
  *more* eagerly than `split_command_chain` (pipes and substitutions too) because agy matches its
  own deny rules against the whole line and never decomposes a chain — `echo probe; git push`
  slips past an anchored rule, and closing that is the entire reason this runs as a hook.
  Per-project overrides: `<workspace>/.agy/policy.toml` (`deny`/`allow` prefix lists), found via
  the payload's `workspacePaths` — deliberately **not** `<ws>/.agents/hooks.json`, which would put
  the guard in the working tree that grounds `wrote:`.
  **A here-string body is skipped** (`@'…'@` / `@"…"@`), **closed regions only**: otherwise the
  `\n` split hands the rules each line of a document being written, and an unterminated `@'` would
  hide a whole chain. **Destructive git is refused** — `git restore`, `git checkout <path>`,
  `git stash`, `git reset --hard`: a sub-agent is never the right process to decide an edit it did
  not make should cease to exist. **An unparseable payload allows** (parse error logged): no
  command to judge, no basis to refuse. `check-command` **always exits 0** — a non-zero exit is a
  hook *failure*, and agy fail-closes on it by blocking every tool. Reasons are ASCII-only (they
  cross into agy and then the model; invariant #2).
  **It governs delegated runs ONLY.** `hooks.json` is shared with the user's desktop app, IDE and
  terminal sessions; `run_agy` marks its own child with `AGY_MCP_DELEGATED=1` (agy passes its
  environment to hook processes) and anything without it is waved through with
  `interactive session`. A person at the keyboard chose the permission mode and can answer a
  prompt; the guard exists for a run **nobody is watching**.
  **The registered path is a pinned private copy** (`~/.agy-mcp/guard/agy-mcp-guard.exe`, written
  by `install-hook`), *not* `~/.cargo/bin` or `target/release`: fail-closed plus a shared hook file
  means a missing binary breaks the user's own sessions, so neither `cargo build` nor
  `install.ps1` may be able to cause it. It cannot be quoted (agy escapes the quotes into
  `cmd /c`), so a spaced path falls back to the 8.3 short name.
  **Per-run policy comes from the dispatching agent**: preset keys `policy_deny`/`policy_allow`
  are serialized into `AGY_MCP_POLICY` for that one agy process. Layering:
  `~/.agy-mcp/policy.toml` → `<ws>/.agy/policy.toml` → the preset, unioned, `allow` beating
  `deny`. The sub-agent cannot forge either variable: agy spawns the hook from the environment agy
  was launched with.
- **`src/nest.rs`** — no nesting (D17, retiring D11). Every agy we spawn (`run_agy`, `agy models`,
  `/usage`, `--version`) carries `nest::spawned_env()` (`AGY_MCP_DEPTH`). agy starts the agy-mcp in
  its shared `mcp_config.json` under each of them, and `main` returns before the handshake when
  `inside_spawned_agy()`: no tools, no startup maintenance, no `agy models`. Unmarked, that server
  refreshed the model cache at startup and its agy started the next one (the 2026-10-29 process
  loop). Never spawn agy without `spawned_env()`. `AGY_WORKSPACE` still goes to a run's agy for an
  `agy-mcp <subcommand>` it shells out to.
- **`src/agy/agents.rs`** — custom agents for presets (D12). A preset's `agent` reaches
  `RunOpts::agent`; `run_agy` adds `--agent <name>` on a **fresh** run only (a resume keeps the
  agent's tool set) and first calls `ensure_builtin_agent`, which rewrites
  `~/.gemini/config/agents/<name>.md` from the copy in the binary (`src/presets/agents/`) when it is
  missing or differs. A name agy-mcp does not ship passes through untouched; agy refuses an unknown
  one with exit 1, so a missing agent never runs as the default agent. The agent narrows agy's
  built-in tools, not MCP (`mcp="none"` does that).

## The 3 tools — implementation notes

**Caller-visible behavior is canonical in [`REFERENCE.md`](../REFERENCE.md)**: args, return
shapes, messages, env vars and their defaults, the footer format. This section keeps what a code
change must respect — which function owns each decision, and what must not come back. Prose
writes "`agy_diag(trace)`" for `agy_diag(action=trace)`.

- **The schema is a cost, so anything with a sound default is not an arg.** The always-loaded
  `tools/list` is **3166 chars over 3 tools**. `effort`, `gemini_ver`, `collect`,
  `max_answer_chars` and `timeout_s` went — the caller had no better answer; the last two survive
  as `AGY_MAX_ANSWER_CHARS` and **`AGY_TIMEOUT`** for probes, and backgrounded-task capture is
  always passive (the disk task-log read is ground truth). `strip_defaults` drops every top-level
  `default` under `properties` (Claude Desktop's Code tab rejects an omitted optional arg that has
  one; `#[serde(default)]` still deserializes, `nullable` is kept); `normalize_opt_arg` maps
  `"null"`/`""` to `None`.
  **Before adding an arg, ask whether a default plus an env var would do** — and before adding an
  *action*, whether it reads local state only, in which case it belongs in the CLI.
- **Model decisions at dispatch.** `model` is tried as one of the four aliases first
  (`SubModel::from_alias` → `resolve_sub_refreshing`), anything else — and every `model_label` —
  as a name through the pure `resolve_by_name` (exact slug → `family_key` → the one version-less
  line every word names; two lines is an error listing them; a named effort is exact, otherwise
  `slot_effort`; a miss falls through to `validate_model_label`'s catalog-miss message). An empty
  cache passes the name through (invariant #4); a miss re-lists once before it is believed
  (`resolve_by_name_refreshing`). A fresh run with no model goes through `resolve_default_model`
  (`AGY_DEFAULT_MODEL`, else `flash`, else refuse — never a guess). `resolve_gemini`/`resolve_claude` return the const **only when the cache is empty**, and
  share `pick_version_effort` (newest version, then the effort ladder — Claude carries the effort
  suffix since 5.5);
  a non-empty cache missing the slot returns None and `resolve_sub` errors instead of shipping a
  stale label. A bare resume keeps the conversation's model through `resume_keep_model`
  (pure core `resume_keep_from`; non-Flash only, a slot-less model by `family_key`; a
  Flash/unreadable session → no `--model`). Every
  explicit `--model` is checked after the run by the pure `agy::slot_mismatch(requested, served)`
  — **no** `expect_switch` param, slot-level, or by `family_key` when a side has no slot (both
  must start `gemini|claude|gpt`, else no warning);
  the same classifier feeds `agy-mcp stats`'s mismatch-rate block. `DispatchSpec::prompt_head` is
  cut from the raw prompt **before** `with_preamble`, else every job quotes the preamble.
- **Effort is internal, never per-task tuned.** `effective_effort` is a fixed per-model floor
  (history: design-notes). **Do not re-introduce an `effort` arg**: a schema field invites
  per-task effort picking, which the user's standing instruction forbids. The override is a
  name ending in the effort (`model="flash-low"`).
- **Context is REPORTED, never enforced.** No refused resume, no mid-run kill
  (`StopReason::CtxCap` is gone): both spent finished work on a **quality** risk the caller judges
  better from the answer. `ctx_limit_for` (pure: **Flash only**, `force=true` opts out) decides
  *whether*, carried as `DispatchPlan::ctx_limit`; after the run `run_core` applies the pure
  `ctx_over_note`; on a resume `plan_dispatch` puts the same sentence in `ctx_warn` up front, and
  `ctx_warn` wins so it never appears twice. **Neither names a model to switch to — do not
  re-introduce a recommendation** (long-context degradation hits every line; evidence:
  design-notes). **Do not re-introduce the watchdog**: `should_stop` takes idle and hard-cap only,
  and `RunOpts` carries no ctx field, so `src/agy/` has no opinion about context size. The generic
  warning compares `session_ctx_tokens` with `CLAUDE_CTX_CEILING`.
- **Presets are data, not code (`src/presets.rs`).** Built-ins ship as
  `src/presets/builtin.toml`, **written in the user's own dialect and parsed by the same code**.
  **This repo dogfoods it** in `.agy/presets.toml`; rationale: `docs/ordering-conventions.md`.
  **Failing loudly is the whole design** — a silent fall-back to no preamble reads as "the model
  got worse". The default (`DEFAULT_PRESET`) is the ordinary entry `none`. All but `load_table` is
  pure and unit-tested.
  **`env`**: `validate_preset_env` runs per layer **before** merge; `resolve_preset` merges
  parent-first, child winning per key; values go through `substituted_env` and are applied to the
  agy child / verify `Command` FIRST, so the bridge's control vars (`DELEGATED_ENV`,
  `AGY_CLI_DISABLE_AUTO_UPDATE`, `POLICY_ENV`, `PATHEXT`) always win.
  **`verify` is the one thing a preset makes the BRIDGE do**, and deliberately **not** an
  `agy_sub` arg: the check is a property of the project, not the call. Its gate is
  `presets::verify_command_is_safe`, **reusing the sub-agent's `is_safe_command_segment`**, one
  segment only (a chain needs a shell or a reimplementation of `;`/`&&`/`||`); the output folds
  into the footer via the pure `summarize_verify_output`. The program is resolved by the pure
  `resolve_program` (PATH × PATHEXT) because `CreateProcess` cannot run a `.cmd`; `pathext_fixup`
  does not cover this — it repairs the CHILD's environment, and the failure is the parent's own
  lookup. That `cmd /c` is not the shell the policy avoids: the string already cleared
  `verify_command_is_safe` (one segment, restricted charset). When `rc.exe` does not resolve on
  PATH, the newest Windows SDK `bin\<ver>\x64` is appended (pure `newest_sdk_version` /
  `path_with_sdk_bin`): a client-started server's PATH lacks the SDK and `tauri-winres` panics.
- **A bare non-zero exit (no `AGY_ERROR` receipt) names a broken image** the prompt points at: pure
  `image_paths_in_prompt` / `image_looks_complete` (signature + end marker, no image crate, D14) /
  `broken_image_note`, called from `run_agy_once`'s `exit_err`.
- **`research`/`report` findings land OUTSIDE the workspace** (`agy::reports_dir()`), pushed onto
  that run's `add_dirs`: in the repo the git snapshot would make a read-only investigation report
  as a change. Deliberately **not** an arg.
- **`agy_diag(status)`**: the newest-transcript check uses the pure `conv_was_denied`
  (`src/agy/status.rs`) — a `failed` job record for that conversation whose `DeniedError` text
  survived into `result` — so a gate refusal is not reported as the trust/OAuth fault
  (invariant #7). The workspace row is `workspace_status_line` (invariant #5).
- **Jobs and the CLI.** The in-memory registry is pruned at >50, evicting `seen=true` finished
  jobs first (`jobs_to_evict`), so the cap can't drop an uncollected result; a miss falls back to
  the disk mirror (`~/.agy-mcp/jobs/<id>.json`, >24h pruned). **`seen` lives on the `JobRecord`
  (disk)**, not only the in-memory `Job`: the recommended collector (`agy-mcp wait`) is another
  *process*. `mark_job_seen` is the shared writer; `pending_jobs_banner` cross-checks the mirror;
  an inline return calls `mark_seen` for ok and error alike (load-bearing).
  `main()` dispatches `agy-mcp <subcommand>` to a one-shot CLI reading only local state; with no
  args it serves stdio. Deliberately **no dispatch subcommand** (quota, concurrency and MCP
  scoping live in the server). `agy-mcp doctor` is the sanctioned way for a session missing
  `agy_*` tools to find out why — not a hand-rolled stdio JSON-RPC client. `wait` with no id uses
  the pure `select_wait_targets`, scoped by `same_workspace_path` against the cwd; **`AGY_WORKSPACE`
  wins over the cwd** (below it, it is unreachable: `current_dir()` effectively always succeeds).
  Its default ceiling is the pure `wait_run_ceiling`/`job_past_ceiling`; a lost job's text is the
  pure `format_lost_job_text`, shared by the startup orphan sweep and wait's inline path; it
  lists the job's half-written files from `JobRecord::written_paths` (receipts newer than the
  job's start, read from the transcript).
  **The job record IS the dispatch spec**: the `conversation_id` is written *mid-run*
  once the conv dir is detected (`RunOpts::job_id` → `note_job_progress`), and every failure
  carries `JobRecord::recovery_hint`. **Stall visibility**: a run mirrors agy's liveness into
  `last_activity_unix` every 30s; run caps use the **wall clock** (`wall_since`), not `Instant`,
  which can freeze across a suspend (design-notes). The running line's step and pace come from
  the pure `last_step_line` and `median_step_interval_s` over `recent_step_timestamps`.
- **Instances coexist — there is no single-instance guard.** Claude Code spawns an MCP server **per
  session**; refusing startup left later sessions with **no `agy_*` tools at all**.
  `register_instance` only announces the pid under `~/.agy-mcp/instances/` for `doctor`. Shared
  state is guarded where it lives: job ids are claimed atomically on disk (`claim_job_id` via
  `create_new`; a per-process counter collides across servers), `mark_orphaned_running` demotes only
  records whose `owner_pid` is dead (pure, unit-tested `should_demote_job` — **never fail a live
  sibling's job**), and the MCP-config swap claims its `.agyswapbak` breadcrumb with `create_new`
  while `recover_mcp_swap` skips one whose owner is alive.
  **Diagnosis path when a session reports no `agy_*` tools: it is almost never the config.** Check
  `Get-CimInstance Win32_Process -Filter "Name='agy-mcp.exe'"` and the `~/.claude.json` registration
  first. The CLI is not a fallback — there is **no dispatch path outside MCP** — so no MCP means no
  delegation at all. Say that, rather than silently absorbing the work in-session.
- **Shared working tree at dispatch.** `shared_tree_note_for` (disk: job records, and each running
  sibling's receipts via `read_full`) feeds the pure `running_in_workspace` — which reuses
  `should_demote_job`, so a dead owner's record is not "running" — and `shared_tree_note`. It is
  taken before this job's own record exists, so it names only siblings.
- **Report footer** (`footer_bits`/`with_footer`). Artifacts are **paths only, never bodies**
  (sourcing: design-notes). Line deltas: `line_count_note`; EOL flips: `dominant_eol` (a byte
  scan — Git Bash's grep drops CR); repeats: `repeated_command_warning` (8+); left-behind files:
  `stray_artifacts`/`looks_stray`, attributed to THIS run only; writes outside the run's `scope`:
  `outside_scope`, receipts only (D13: flagged, never blocked). Artifacts under `agy::brain_dir()`
  (`implementation_plan.md`/`task.md`) are **filtered out** — they would make a punt look
  productive.
- **A failure reports what it got done first.** The pure **`progress_note`**; `resolve_outcome`
  attaches it (via `.context`) to **every** error path; `run_core` adds `changed on disk:` from
  the post-run snapshot. **The run still failed** — status, `seen`/banner and `wait`'s exit 1 are
  unchanged. The soft-deny loop downcasts to `DeniedError`, so a test pins the `.context` wrapper
  transparent to `downcast_ref` — **if it isn't, auto-granting silently stops**.
- **`wrote:` is the transcript's OWN write receipts, never the git diff** — the diff credits a run
  with concurrent edits by siblings or the supervisor (history: design-notes). `run_core` takes a
  `git_snapshot` before and after and keeps the diff (`git_changed_paths`) **separate**:
  `git_only_paths` (pure; `norm_path_for_match` folds separators and Windows case) is the
  unreceipted remainder, and when it is non-empty `workspace_concurrency_note` /
  `agy::overlapping_jobs` (pure; a **still-running sibling ends "now or later"**) names who else
  ran. **Snapshot rules:** fingerprint (`parse_porcelain_with`) **every** listed path, tracked or
  not — else a file dirty before and after compares equal though rewritten (false punt nudge).
  `parse_porcelain` handles renames + quoted paths; the status code is **trimmed**, so a pure
  `git add` is deliberately not a write. Run with **`-c core.quotepath=false`** and still
  C-unquote (`unquote_porcelain_path`, octal escapes included) — else a non-ASCII path prints as
  mojibake and `metadata()` misses. Non-git → `None` from `git_snapshot`: an empty diff from a
  non-repo must **never** read as "changed nothing".
- **Punt nudge (`punt_evidence`, `punt_nudge_text`).** **Any receipt silences it.** It
  **deliberately does NOT fire** on no receipts + non-empty diff + nothing overlapping: that is
  usually a shell-made edit agy doesn't receipt — real work. **Known blind spot:** the supervising
  session's own edits are not jobs, are never named, and can silence the nudge on a run that did
  nothing.
- **`agy_diag(live)`**: `quota` spawns `agy -p /usage --output-format json` (plus `/credits`) and
  parses with the pure `parse_usage_json` (`reset_time` → seconds via `iso8601_utc_to_unix`);
  `models` runs the headless `list_model_slugs`. Both answer in seconds with no quota, so the call
  takes **one** permit and awaits directly — no exclusive hold, no detached task.
- **Quota knowledge accrues by itself — "when do I check quota?" is not the caller's decision.**
  Two feeds keep `~/.agy-mcp/quota_log.jsonl` current. (1) **`record_quota_wall`**: a quota-wall
  failure (pure `is_quota_wall`: `RESOURCE_EXHAUSTED` / `code 429` / `[429]` / a 429 inside an `AGY_ERROR` receipt) logs a 0% snapshot
  for the group (the receipt *is* an authoritative reading), so the next dispatch refuses up front.
  Group = **requested model, else the conversation's served model** (`resolve_quota_group`, via
  `read_conv_model`) — a **bare resume** passes no `--model`.
  `cached_quota_alternative` names the OTHER group's model (`opus` for a Gemini wall, `flash` for
  a Claude wall) unless that group is itself exhausted in either window.
  (2) **`maybe_refresh_quota`**: after every run (in `spawn_job`, permit released), reads `/usage` only
  if the newest reading is missing or >30 min old (`quota_needs_refresh` → pure
  `quota_cache_is_stale`) **and** `try_acquire_owned` gets a permit — try, **never
  `await`**: a queued refresh could delay a real dispatch.
  **Both windows refuse** (`exhausted_window`, weekly checked first): an exhausted week logs
  `weekly: 0` and no 5h bucket at all, so a 5h-only guard let every run through to a 429. The
  refusal is the pure `quota_unavailable`; its first line is fixed so a caller's rule can match
  it. `QUOTA_EXHAUSTED:` stays the *post-spawn* failure (the 429 was paid); `wall_window_note`
  adds the window when a `/usage` reading names one.
  **A reading stops blocking when something newer contradicts it** (pure `effective_pct` /
  `parse_cached_pct`): a **successful run** on the group in `runs.jsonl` after it
  (`ok_run_since` — invalidate, **never write a made-up %**; the burn estimate reads it), or a
  **`/usage` reading that parsed but lacked the group** (logs `"missing": [group]`); else a 0% refuses for
  the full 4h after a refill. **A `/usage` reading carries its refresh time** (`reset_time`
  → `5h_reset`/`weekly_reset`, absolute unix time): dropped once passed, and a quota-wall run
  calls `maybe_refresh_quota(force=true)` to fetch it. Thresholds live once, in `quota.rs`
  (`QUOTA_EXHAUSTED_PCT`, `QUOTA_CACHE_FRESH_S`, judged by `CachedPct::is_exhausted`).
- **The state reaches the caller before its first call, through `instructions` only.**
  `get_info` appends the pure `status_line` (cache only), and `initialize` then kicks
  `maybe_refresh_quota(false)` — never awaited. **Tool descriptions never change mid-session**
  (invariant #9).
- **`AGY_DOWN:`** rides on `refresh_models_cache`: the startup call runs even with a fresh models
  cache, and its failure is kept for `agy_down` (15s timeout; a hang leaves the verdict as it
  was). `agy_sub` re-probes before refusing. Assumed, not yet observed: a signed-out agy fails
  `agy models`.
- **`agy_diag(trace)` / `agy_diag(sessions)`** read the disk transcript only: no agy launch, no
  quota, no lock. Ids accept a prefix (`agy::resolve_conv_dir`).
- **`agy-mcp stats` / `agy-mcp quota` are CLI, off the schema** — pure reads of
  `~/.agy-mcp/*.jsonl`; a schema entry is paid in **every** session. *Don't carry the context for
  looking, but keep looking cheap.* The **live** read stays MCP: it spawns agy inside the server's
  semaphore, which a CLI process cannot take. Stats rows group by the REQUESTED model (pure `stats_group_label` + `canon_model`
  → `model_stat_rows`; `canon_model` merges label and slug spellings). Deliberately **not**
  `slot_of` — too coarse to compare sonnet against opus. The mismatch block is
  `requested_mismatch_stats`; the cached report is `cached_quota_report`.
- **Burn rate refuses to guess.** `burn_estimate` (pure) uses **every** snapshot pair in 24h,
  **drops intervals where the percentage went UP** (refilled; counting those runs as free drags
  the rate toward zero), and below `MIN_BURN_RUNS` (5) `format_burn` says
  `not enough measured runs` — a confident number from too few runs was the bug. The weekly
  projection is `weekly_runs_left`, under the same minimum.
- **`agy_sub`'s `mcp` arg**: no per-run agy flag, so `run_agy` swaps the shared
  `~/.gemini/config/mcp_config.json` via `apply_mcp_swap` + RAII `McpGuard` (Drop restores on
  error/timeout/cancel); `recover_mcp_swap` restores a crash leftover (`.agyswapbak`) at startup.
  Only parse/swap a config we could read — **never corrupt a file we don't understand**.
  Concurrency: design-notes. **Global file only** — the bridge never swaps inside the working tree.
  **Visibility, not repair** (`src/agy/customizations.rs`): agy drops a skill with unparseable
  frontmatter and says so only in its own log, so the bridge counts and names them
  (`broken_skills_warning`, `mcp_scope_note`). **`frontmatter_problem` encodes only measured
  drops**: no `---` block, an unterminated one, an unquoted value containing `: ` or ending in
  `:`. A missing `name` or `description` is **not** a drop (agy loads it) and is not flagged — a
  warning on every answer must **never be a guess**. No YAML crate; add a rule only with the probe
  showing agy drop it. Skills de-dup by canonical path (the global `zenith` links to a directory
  `skills.json` lists).

## Headless permission gate (full)

- **Headless permission gate.** agy soft-denies any tool needing confirmation;
  `--mode accept-edits` does NOT help. The persisted `toolPermission` setting decides (default
  `request-review`), and **print mode honors it** (escape hatches, hook design: design-notes):
  - **Pre-granted families, as `regex:` rules.** `ensure_verification_commands_allowed` (once at
    MCP-server startup) permanently merges `verification_command_grants()` into
    `~/.gemini/config/config.json`'s `globalPermissionGrants.allow`. agy matches a rule
    **exactly** unless it starts with `regex:`, so the list is **patterns, not 40 strings**, per
    family (`cargo`, `npm|yarn|pnpm`, `npx`, `pytest`, `mvn|gradle`, `go test|build`,
    `dotnet test`, `python -m pytest`, `where`/`which`, `<known tool> --version`, `$env:NAME`).
    **The regex list is a FAST PATH; `is_safe_command_segment` is the POLICY.** The invariant is
    **containment, not equality**: every line a pattern matches must also pass
    `is_safe_command_segment` — narrower is fine, wider is a bug (a unit test asserts it over a
    command corpus with a real RE2 engine). A miss costs **one** recovery round-trip and is then
    granted by the policy, so **do not "fix" a fast-path miss by widening the regex.**
    **Narrowing a pattern requires retiring the old one (`RETIRED_GRANTS`)**: the merge is
    append-only, so an old wider entry keeps matching and the new policy never gets a say. The
    pure `prune_retired_grants` runs *before* the merge in `process_config_json` and removes
    **only the exact historical strings this bridge itself shipped** — never a sweep of `regex:`
    grants, a runtime auto-grant, or anything hand-added. Its entries are frozen literals: don't
    refactor them into `command_pattern(..)` calls; they must match what an old binary wrote. A
    test pins that no retired string is still shipped (else it'd be pruned and re-added forever).
    **What the policy refuses (`denied_subcommand`, per family)** — effects reaching **outside**
    the checkout, since a build/test grant already runs its code (`cargo test`): publishing
    (`publish`/`unpublish`/`deploy`/`release`), registry credentials
    (`login`/`logout`/`adduser`/`token`/`owner`/`access`/`yank`), installing onto PATH
    (`cargo install`, the npm family's `install|add|i` with `-g`/`--global`/`--location=global`).
    Per family: `npm install` is an in-project step, `cargo install` is not. `npx` stays fully
    allowed — indistinguishable from `npm install` running postinstall scripts.
    **The design rests on three measured facts:** (1) exact rules do not argument-prefix-match
    (`command(git status)` did *not* admit `git status --short --branch`, so no exact list is ever
    complete); (2) a `regex:` rule must **fully match** (`regex:^Get-Random` did *not* admit
    `Get-Random -Maximum 5`); (3) agy matches it against the **whole command line
    without decomposing a chain** (`^Get-Date.*` admitted `Get-Date -Format o; Get-Location`).
    So **the argument character class is the entire safety property**:
    `RE_ARG` excludes `;` `&` `|` `<` `>` `$` `(` `)` `` ` `` and quotes, which is why
    `cargo build; rm -rf x` cannot match a `cargo` pattern. Anchoring and family coverage are
    unit-tested against RE2 (`regex`, a **dev-dependency** — nothing linked into the binary). File
    writes are granted with the trust entry by `ensure_workspace_trusted` (`write_file(<dir>)`
    under `permissions.allow` in settings.json). Merge-and-leave-permanent, deliberately not an
    inject/restore like `McpGuard`: a swap would replicate the cross-process race for no benefit.
  - **`AGY_PERMISSIONS=trusted` — the setup-time escape hatch.** On the `claude mcp add` line
    (`-e AGY_PERMISSIONS=trusted`) it passes `--dangerously-skip-permissions` to every agy spawn.
    Deliberately **not** a tool arg or per-call: un-gated sub-agents are the operator's standing
    decision. Pure `parse_permission_mode`; an unrecognized value **warns and keeps the gate** (a
    typo must never take permissions off); `agy_diag(status)` states the active mode.
  - **`agy-mcp install-hook` — the only option that keeps a check.** `AGY_PERMISSIONS=trusted`
    puts nothing back; `toolPermission=always-proceed` removes the gate machine-wide, and the guard
    (`src/hookguard.rs`) makes that defensible — no run dies at the gate, the denylist still
    refuses the destructive/outward-facing set, interactive sessions included. **Opt-in, and
    `install.ps1` deliberately does not run it** (a standing decision, not a deploy step). With the
    gate still on the guard is never consulted, because agy soft-denies first.
  - **Soft-deny auto-recovery (grant + resume, up to a budget).** `run_agy` loops over
    `run_agy_once`; on a denial it asks the pure **`auto_grant_rule`**. A **command** is split by
    `split_command_chain` on `;` / `&&` / `||` (they only *sequence* lines) and is grantable only
    if **every** segment passes `is_safe_command_segment`: the build/test/lint family, a read-only
    discovery probe (`where`/`which`, or `<known tool> --version` — `DISCOVERY_BINS`, an explicit
    list so it never means "run an arbitrary binary"), or a name-shaped `$env:NAME` read. The
    **first token is normalized by the pure `command_basename`** (directory and `.exe` stripped),
    so `C:\…\.cargo\bin\cargo.exe test` (the path `where cargo` found) classifies like
    `cargo test`; name-only, it does **not** widen the family (a non-family program stays refused, qualified or not).
    Pipes, redirects, `$(`, backticks and a backgrounding bare `&` stay a **hard reject** —
    the family check can't see through them, and `cargo build; rm -rf x` must keep failing. The
    granted rule is the **whole original line** (grants are exact-string matches). A
    **`read_url`** is grantable for the exact URL only, http/https, no credentials in the authority;
    it needs `Url` in `parse_last_tool_call`'s key list, since agy's denial names a literal
    `<target>`. **write_file/read_file** only inside the run's own workspace. Anything else → None
    → the original error stands. A grant calls `grant_permission_entry` (permanent), resumes the
    SAME conversation and prefixes
    `↻ auto-granted <rules> after headless permission denial(s); resumed`.
    **Budgeted, not single-shot** (`AUTO_GRANT_BUDGET` = 4, env
    `AGY_AUTO_GRANT_BUDGET`): an agent issues a *sequence* of distinct commands. Three independent
    termination guards: the budget, `auto_grant_rule` returning None, and the **no-progress rule**
    (a rule denied twice → stop and say so, never retry it). The latest attempt's outcome,
    error included, is returned.
  - **Prevention beats recovery**: `prepend_workspace_directive` (every `agy_sub` run) tells the
    sub-agent to run commands directly, never wrapped in `powershell -Command`/`cmd /c`, and to use
    its own file tools, never an **inline script runner** (`python -c`, `node -e`), to
    read/search/summarize files — neither can ever be auto-granted, and models reach for the
    wrapper unprompted. Chaining is recoverable when every segment is safe, so it is not
    forbidden. `preset=code` adds that each distinct command line costs a recovery round-trip.
  - Both writers of `config.json` share **one** `config_json_lock`.
