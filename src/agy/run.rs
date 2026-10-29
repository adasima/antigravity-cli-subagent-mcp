use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};
use tokio::process::Command;

use crate::agy::jobs::note_job_progress;
use crate::agy::mcp::{apply_mcp_swap, McpMode};
use crate::agy::models::wire_model;
use crate::agy::paths::{
    agy_bin, brain_dir, brain_dir_names, conversations_dir, newest_dir_not_in,
};
use crate::agy::permissions::{
    auto_grant_rule, ensure_workspaces_trusted, grant_permission_entry, parse_denied_permission,
    DeniedError, PermissionMode,
};
use crate::agy::quota::now_unix;
use crate::agy::transcript::{
    clip, find_test_summary, read_full_since, read_last_step, read_last_tool_call,
    session_ctx_tokens, step_baseline, strip_broken_encoding, strip_system_blocks, TaskResult,
    Transcript,
};

pub struct AgyOutcome {
    pub text: String,
    /// Bare answer as agy wrote it (before folding in background task results
    /// or prepending interruption notes). `is_wait_only` decisions must be made
    /// against this, not `text`, because appended task results make `text` exceed
    /// the wait-only length cap and hide the waiting state.
    pub bare_answer: String,
    pub conv_id: Option<String>,
    /// Where the answer came from: "stdout" or "transcript".
    pub via: &'static str,
    /// Model label agy reported actually serving (from the transcript receipt).
    pub served_model: Option<String>,
    /// Files the sub-agent created/modified (paths only; from the transcript).
    pub artifacts: Vec<String>,
    /// One-line tool-call tally for the report footer.
    pub step_summary: String,
    /// Rough context size after this run (tokens ≈ transcript bytes / 4), so the
    /// caller can weigh resuming this session vs starting fresh. 0 if unknown.
    pub ctx_tokens: u64,
    /// Background tasks (e.g. `cargo build`) that completed out-of-band after agy
    /// ended its turn — drained and captured by run_agy. Empty for normal runs.
    pub task_results: Vec<TaskResult>,
    /// True when this Ok outcome is a partial answer with no final report — agy's
    /// own `--print-timeout` (see `agy_truncation_note`), or the bridge's idle/
    /// hard-cap stop. Never true for `StopReason::QuotaWall` (that stop is a
    /// failure — see `stop_forces_failure` — so it never reaches an Ok outcome).
    /// Drives the one auto-resume-for-a-summary in `run_core`.
    pub cut_off: bool,
}

/// Options for a single `agy -p` run.
pub struct RunOpts<'a> {
    pub prompt: &'a str,
    pub workspace: &'a Path,
    /// Model catalog slug (e.g. "gemini-3.1-pro-high") or display label. Converted
    /// to display label at the agy boundary via `wire_model`. None => agy's
    /// configured default model.
    pub model: Option<&'a str>,
    /// Extra directories to grant agy (repeatable `--add-dir`). Use this to
    /// attach files: add the file's parent dir and reference the path in the
    /// prompt.
    pub add_dirs: &'a [String],
    /// Resume a prior conversation (for follow-ups).
    pub continue_id: Option<&'a str>,
    /// Hard ceiling (s) on the whole run — also passed to agy as `--print-timeout`
    /// as a backstop. NOT the primary governor: a run is normally stopped by the
    /// liveness ("heartbeat") idle timeout below, so this can be generous.
    pub timeout_s: u64,
    /// Idle timeout (s): kill agy only after this long with NO conversation-store /
    /// brain-dir activity. This is what lets agy WAIT on a long `cargo build` it
    /// shelled out without being cut off mid-build (a fixed total wall-clock used to
    /// kill productive runs the moment they hit the budget — observed in a real
    /// storage-layer conversion that died at 600s while its build was still RUNNING).
    pub idle_timeout_s: u64,
    /// MCP servers to expose to the sub-agent for this run (swaps the shared
    /// config, restored after the run). Inherit => no swap.
    pub mcp: McpMode,
    /// Serializes the spawn→new-conversation-dir detection window for FRESH runs
    /// so concurrent fresh launches each capture their own dir (resumes pass the
    /// id directly and don't need it). None => no serialization (single-run path).
    pub spawn_gate: Option<&'a tokio::sync::Mutex<()>>,
    /// Guards the window in which agy reads the SHARED `mcp_config.json`.
    ///
    /// Ordinary runs take the **read** side across `spawn()` alone — they don't
    /// contend with each other, so this costs nothing. A run with a custom `mcp`
    /// scope takes the **write** side for the swap. That is the whole reason a
    /// scoped run used to need every concurrency permit: not the swap's duration,
    /// but the fact that any agy STARTING while the file is swapped would read the
    /// wrong scope. Bounding that precisely lets a scoped run take one permit like
    /// anything else. None => single-run path, no coordination needed.
    pub swap_gate: Option<&'a tokio::sync::RwLock<()>>,
    /// Job this run belongs to, if any. Given one, the run writes its
    /// conversation id and liveness heartbeats into the disk mirror, which is
    /// what makes a job survivable (resumable) and a stall visible while it is
    /// still running.
    pub job_id: Option<&'a str>,
    /// Where to report "still working" while the run is in flight. None = nobody
    /// asked. See [`ProgressSink`].
    pub progress: Option<ProgressSink>,
    /// The dispatching agent's guard policy for this run, as JSON, handed to the
    /// PreToolUse guard through agy's environment. `None` = whatever the layered
    /// policy files say.
    ///
    /// This is also where the run is marked **delegated**: agy passes its
    /// environment down to hook processes, so the guard can tell a run nobody is
    /// watching from a person driving agy themselves — and only the former is held
    /// to a policy. The sub-agent cannot forge it: the hook is spawned by agy from
    /// the environment agy was launched with.
    pub guard_policy: Option<String>,
    /// Extra env for the agy child process, from the dispatched preset's `env`
    /// table (already `{workspace}`/`{reports}`-substituted and validated at
    /// preset-resolve time — see `presets::validate_preset_env`). Applied to the
    /// `Command` BEFORE the bridge's own vars below (`DELEGATED_ENV`,
    /// `AGY_CLI_DISABLE_AUTO_UPDATE`, `POLICY_ENV`, `PATHEXT`), so the bridge's own
    /// values always win even if a validator bug ever let a colliding key through —
    /// the preset's env must never be able to disable the hook guard or the
    /// auto-update pin. Empty when the preset sets none.
    pub preset_env: &'a BTreeMap<String, String>,
    /// Custom agent for a fresh run (`--agent`). Ignored on a resume: the
    /// conversation keeps the agent's tool set without the flag (measured).
    pub agent: Option<&'a str>,
}

/// A best-effort "still working" ping to whoever is waiting on this run.
///
/// Deliberately a plain callback and not an rmcp type: `src/agy/` carries no
/// protocol crate (see `mod.rs`), and all this module needs to know is that
/// someone may want to hear a heartbeat. `main.rs` turns each call into a
/// `notifications/progress`. It takes the tick number (MCP requires a
/// monotonically increasing value) and a human-readable line.
///
/// Two things depend on it, and only the second is cosmetic:
/// 1. **A long call is aborted by the client after 30 minutes of silence** — a
///    response or a progress notification is what resets that timer, and a run
///    that is genuinely busy for that long sends neither on its own.
/// 2. Because the call now blocks until the run finishes, without this the
///    session sits mute for the whole run (measured: a 129.6s run).
pub(crate) type ProgressSink = std::sync::Arc<dyn Fn(u32, String) + Send + Sync>;

/// Determine if the final answer is practically empty (just a waiting message indicating
/// that the agent is waiting for a background task to finish, without summarizing the changes).
pub fn is_wait_only(text: &str) -> bool {
    let text_trimmed = text.trim();
    if text_trimmed.is_empty() {
        return false;
    }
    // Waiting messages are generally short. If it exceeds 300 characters, it likely contains
    // some actual summary or detailed explanation, so we treat it as not wait-only.
    if text_trimmed.chars().count() > 300 {
        return false;
    }

    let text_lower = text_trimmed.to_lowercase();

    // Check if it contains keywords representing "waiting".
    const WAIT_KWS: &[&str] = &["wait", "waiting", "monitor", "expect", "待", "監視"];
    let has_wait_kw = WAIT_KWS.iter().any(|&kw| text_lower.contains(kw));

    if !has_wait_kw {
        return false;
    }

    // Check if it contains keywords representing "implemented changes" or "work details".
    // If it contains these, it's not merely a waiting template, but has some content summaries.
    const WORK_KWS: &[&str] = &[
        "implement",
        "modify",
        "update",
        "edit",
        "fix",
        "solved",
        "change",
        "add",
        "create",
        "delete",
        "実装",
        "修正",
        "変更",
        "追加",
        "作成",
        "削除",
        "解決",
    ];
    let has_work_kw = WORK_KWS.iter().any(|&kw| text_lower.contains(kw));

    !has_work_kw
}

/// Why the bridge stopped agy, when it was the bridge that stopped it (as opposed
/// to agy exiting on its own).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum StopReason {
    /// No conversation-store / brain-dir activity for `idle_timeout_s`.
    Idle,
    /// The generous `timeout_s` ceiling, while still active.
    HardCap,
    /// agy repeatedly failing with 429 quota exhaustion retries.
    QuotaWall(usize),
}

impl StopReason {
    /// One-line prefix for an answer recovered from a run the bridge stopped.
    /// Without it a partial answer left in the transcript is reported as a normal
    /// success and the caller has no way to tell the run was cut off.
    pub(crate) fn partial_note(&self) -> String {
        match self {
            StopReason::Idle => {
                "⚠ stopped by the bridge (idle timeout) — the answer below may be partial".into()
            }
            StopReason::HardCap => {
                "⚠ stopped by the bridge (hard cap) — the answer below may be partial".into()
            }
            StopReason::QuotaWall(_) => {
                "⚠ stopped by the bridge (quota wall) — the answer below may be partial".into()
            }
        }
    }
}

/// agy's own mid-turn cutoff, read off stderr. Pure.
///
/// `--print-timeout` used to FAIL the run. agy 1.1.28 changed it to "return the
/// partial output and exit 0, with a warning on stderr" — which means a truncated
/// run is now indistinguishable from a finished one at the exit code and in the
/// transcript, and the bridge would report it as a clean success. Measured
/// 2026-09-09 against agy 1.1.28: exit 0, stdout empty, stderr
/// `[agy] print timeout after 10s with turn in progress; returning partial output`.
///
/// The counterpart of [`StopReason::partial_note`] for a cut the bridge did not
/// make. The bridge's own idle timeout normally fires long before agy's ceiling, so
/// this is the rare path — which is exactly why it must not be silent.
pub(crate) fn agy_truncation_note(stderr: &str) -> Option<&'static str> {
    if stderr.contains("print timeout after") || stderr.contains("returning partial output") {
        Some("⚠ agy hit its own --print-timeout mid-turn — the answer below may be partial")
    } else {
        None
    }
}

/// agy cut its own update stream mid-turn, read off stderr. Pure.
///
/// agy 1.2.1 added an in-process pubsub that kills a subscriber which falls behind,
/// and the subscriber it killed was print mode's own conversation stream: exit 1 with
/// `the connection to the agent was interrupted before the response finished:
/// subscriber fell behind updates, stalled for 6s` (4 of 4 print-mode runs on
/// 2026-09-11; 1.1.28's logs contain no pubsub at all). Nothing the bridge does
/// reaches it, so the useful thing is to name it — without this the failure reads as
/// a bare `exit 1`, and an empty transcript sends the reader after trust/OAuth.
pub(crate) fn agy_stream_interrupt_note(stderr: &str) -> Option<&'static str> {
    if stderr.contains("subscriber fell behind updates")
        || stderr.contains("connection to the agent was interrupted")
    {
        Some(
            "⚠ agy cut its own conversation stream mid-turn (\"subscriber fell behind updates\") \
             — an agy-side fault, not quota, trust or permissions; whatever came back is partial",
        )
    } else {
        None
    }
}

/// The one-line form of agy's headless failure receipt, read off stderr. Pure.
///
/// agy 1.2.6+ ends a failed headless run with `AGY_ERROR: {...}` on stderr (canonical
/// status, HTTP/gRPC code, retryable, error ID per its changelog). Only the minimal
/// shape `{"short_error":"…"}` is confirmed; no real sample has been seen, so **no key
/// name is assumed** beyond ordering the message-ish ones first: every top-level scalar
/// becomes `k=v`, so whatever agy adds (a `429`, `retryable=true`) lands in the failure
/// text as it is. The LAST such line wins (retries print earlier ones). A payload that
/// is not a JSON object is returned as written. `None` when there is no such line.
pub(crate) fn parse_agy_error(stderr: &str) -> Option<String> {
    let rest = stderr
        .lines()
        .rev()
        .find_map(|l| l.trim_start().strip_prefix("AGY_ERROR:"))?
        .trim();
    if rest.is_empty() {
        return None;
    }
    let line = match serde_json::from_str::<serde_json::Value>(rest) {
        Ok(serde_json::Value::Object(map)) => {
            let scalar = |v: &serde_json::Value| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                serde_json::Value::Bool(b) => Some(b.to_string()),
                _ => None,
            };
            let lead = ["short_error", "message", "error", "msg"];
            let mut parts: Vec<String> = Vec::new();
            for k in lead {
                if let Some(v) = map.get(k).and_then(scalar) {
                    parts.push(format!("{k}={v}"));
                }
            }
            for (k, v) in &map {
                if !lead.contains(&k.as_str()) {
                    if let Some(v) = scalar(v) {
                        parts.push(format!("{k}={v}"));
                    }
                }
            }
            if parts.is_empty() {
                rest.to_string()
            } else {
                parts.join(" · ")
            }
        }
        _ => rest.to_string(),
    };
    Some(crate::agy::transcript::clip(&line, 300))
}

/// The measured input ceiling for the Claude lines (`sonnet`/`opus`).
pub(crate) const CLAUDE_CTX_CEILING: u64 = 200_000;

/// The error text for a bridge-initiated stop. Pure so the wording — the only
/// thing the caller ever sees of a stopped run — is testable.
///
/// Every variant has to say the same two things: the file edits are already on
/// disk, and how to continue. The bridge no longer stops a run for its context
/// size (see [`crate::ctx_over_note`]). A quota wall is the one stop whose
/// recovery is a different model, and `run_core` appends which one.
pub(crate) fn stop_message(reason: StopReason, idle_timeout_s: u64, timeout_s: u64) -> String {
    let kind = match reason {
        StopReason::Idle => "idle",
        StopReason::HardCap => "hard cap",
        StopReason::QuotaWall(retries) => {
            return format!(
                "QUOTA_EXHAUSTED: agy repeatedly hit RESOURCE_EXHAUSTED (code 429) \
             ({retries} retries) before writing its final answer — stopped by the bridge. \
             Any file edits it made are already on disk: verify with `git status` / `git diff`."
            )
        }
    };
    format!(
        "agy was stopped ({kind} timeout: no conversation activity for {idle_timeout_s}s, \
         ceiling {timeout_s}s) before writing its final answer — NOT a trust/OAuth issue. \
         Any file edits it made are already on disk: verify with `git status` / \
         `git diff`. For 'idle', the task likely stalled; for 'hard cap', raise \
         AGY_TIMEOUT or split the task. Tune idle via AGY_IDLE_TIMEOUT."
    )
}

/// Whether a partial answer recovered after a bridge stop must still be reported
/// as a failure, even though agy had already written something.
///
/// QuotaWall is the one stop reason where "the run produced text" does not mean
/// "the run succeeded": the quota is exhausted, and until it refills nothing
/// further will happen on its own. Reporting it `done` (observed 2026-09-17:
/// j14/j15 printed `done` with `⚠ stopped by the bridge (quota wall)` at the
/// top) let `agy-mcp wait` exit 0 and a caller checking only the exit code see
/// success. Idle and HardCap are different in kind: those are the bridge's own
/// scheduling call on a run that may otherwise have kept working, and a partial
/// answer there is real, useful output — not evidence the run itself failed.
pub(crate) fn stop_forces_failure(reason: StopReason) -> bool {
    matches!(reason, StopReason::QuotaWall(_))
}

/// Whether an Ok outcome is a partial answer with no final report: agy's own
/// `--print-timeout` (`truncated`, from `agy_truncation_note`), or the bridge's
/// idle/hard-cap stop. `stopped` here can never be `QuotaWall` — that stop is
/// diverted to an Err by `stop_forces_failure` before this is evaluated. Pure so
/// `run_core`'s one-shot "summarize what's unfinished" auto-resume is driven by a
/// tested decision rather than by however the text happens to read.
pub(crate) fn is_cut_off(stopped: Option<StopReason>, truncated: bool) -> bool {
    truncated || matches!(stopped, Some(StopReason::Idle) | Some(StopReason::HardCap))
}

/// One progress line. Pure so the wording is pinned by a test rather than by
/// whatever the last edit happened to leave.
///
/// `silent_for` is `None` while there is **no liveness source yet** — the same
/// state `should_stop` refuses to call idle. Saying "starting" there is the honest
/// reading: we cannot see agy, which is not the same as agy doing nothing.
pub(crate) fn progress_line(elapsed_s: u64, silent_for: Option<u64>) -> String {
    match silent_for {
        None => format!("agy starting · {elapsed_s}s"),
        Some(0..=9) => format!("agy running · {elapsed_s}s"),
        Some(s) => format!("agy running · {elapsed_s}s · quiet {s}s"),
    }
}

/// How often the heartbeat checks liveness while agy runs.
const HEARTBEAT_TICK_S: u64 = 5;
/// How often a running job's liveness is mirrored to its disk record.
const JOB_MIRROR_TICK_S: u64 = 30;
/// How often the caller is told the run is still alive. Well inside the client's
/// 30-minute idle abort, and slow enough not to be noise in a session.
const PROGRESS_TICK_S: u64 = 15;
/// How long the spawn gate is held waiting for a fresh run's conversation dir to
/// appear. Bounded because the gate serializes every other fresh spawn; missing
/// the window is no longer fatal (the heartbeat keeps retrying — see `should_stop`).
const CONV_DIR_DETECT_S: u64 = 12;

/// How long a scoped-MCP run waits, after taking the swap gate's write side,
/// before swapping the config.
///
/// The write lock stops any NEW agy from starting, but an agy spawned a moment
/// earlier has not necessarily read the config yet — measured 2026-08-14, the
/// read lands somewhere between 2s and 6s after spawn. This settle window is that
/// upper bound with margin. It is paid only by scoped runs, which is the right
/// place for it: making every ordinary run hold a lock for 12s to save this would
/// tax the common path for the rare one.
const SWAP_SETTLE_S: u64 = 12;

/// Environment that keeps agy's shelled-out commands from waiting on a human
/// (pagers, git credential prompts). See the spawn site for the incident.
pub(crate) const NON_INTERACTIVE_ENV: [(&str, &str); 3] = [
    ("GIT_PAGER", "cat"),
    ("PAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// The bridge's stop decision for one heartbeat tick.
///
/// `idle_for` is `None` when there is **no liveness source yet** — a fresh run
/// whose conversation dir hasn't been located. Idle must not fire then: "no
/// signal" is not "no progress", and killing on it murdered runs that were
/// working perfectly, then reported them as stalls. Only the hard cap bounds a
/// run in that state.
///
/// Order is deliberate and matches the old inline chain: idle is the cheaper and
/// more actionable signal, so it wins over the hard cap.
///
/// **Context size is deliberately not a stop reason.** It used to be (a mid-run
/// watchdog that killed a Flash run past ~100k tok); it is now reported after the
/// fact by `ctx_over_note` instead. Killing a live run to protect it from its own
/// context throws away finished work to prevent a *quality* problem, and quality
/// is the caller's call to make.
pub(crate) fn should_stop(
    idle_for: Option<Duration>,
    since_start: Duration,
    idle_cap: Duration,
    hard_cap: Duration,
) -> Option<StopReason> {
    if idle_for.is_some_and(|d| d > idle_cap) {
        return Some(StopReason::Idle);
    }
    (since_start > hard_cap).then_some(StopReason::HardCap)
}

/// How many consecutive quota-wall retries (429) agy gets before the bridge kills it.
pub(crate) const QUOTA_RETRY_CUTOFF: usize = 3;

/// How many consecutive quota-wall retries a run gets (`AGY_QUOTA_RETRY_CUTOFF`,
/// default 3). 0 is a valid override — it disables the cutoff entirely, same as
/// `should_stop_quota`'s own `cutoff > 0` guard reads it.
pub(crate) fn quota_retry_cutoff() -> usize {
    std::env::var("AGY_QUOTA_RETRY_CUTOFF")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(QUOTA_RETRY_CUTOFF)
}

/// The pure decision to stop when agy is trapped in a quota retry loop.
/// Without this, agy retries a 429 for up to ~10 minutes while liveness keeps
/// advancing on each retry entry, stalling the run until the idle timeout or hard cap.
pub(crate) fn should_stop_quota(retries: usize, cutoff: usize) -> bool {
    cutoff > 0 && retries >= cutoff
}

/// The heartbeat's quota-wall check: this job's own trailing 429s, read from the
/// steps past `since` (the pre-spawn baseline of a resume; None for a fresh job).
/// Counting the whole conversation killed a resume on the previous turn's wall.
pub(crate) fn quota_wall_stop(conv_dir: &Path, since: Option<i64>) -> Option<StopReason> {
    let retries = match read_full_since(conv_dir, since) {
        Ok(t) => t.trailing_quota_retries,
        Err(e) => e
            .downcast_ref::<crate::agy::transcript::NoAnswerError>()
            .map(|n| n.partial.trailing_quota_retries)
            .unwrap_or(0),
    };
    should_stop_quota(retries, quota_retry_cutoff()).then_some(StopReason::QuotaWall(retries))
}

/// Wall-clock elapsed since `t` (0 if the clock moved backwards). Used instead of
/// `Instant` for the run caps so a machine suspend can't freeze the timers.
pub(crate) fn wall_since(t: SystemTime) -> Duration {
    SystemTime::now().duration_since(t).unwrap_or_default()
}
/// Seconds since the Unix epoch for a `SystemTime` (0 if before the epoch).
pub(crate) fn unix_of(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Captured result of a finished `agy -p` process (replaces `Output` so the
/// heartbeat loop can drain stdout/stderr itself while polling liveness).
struct WaitOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Newest mtime of any file under `dir` (recursive, depth-bounded). Used as a
/// liveness signal for the heartbeat timeout. None if the dir is missing/empty.
fn newest_mtime_under(dir: &Path) -> Option<SystemTime> {
    fn walk(dir: &Path, depth: usize, best: &mut Option<SystemTime>) {
        if depth > 5 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                walk(&e.path(), depth + 1, best);
            } else if let Ok(m) = md.modified() {
                if best.map(|b| m > b).unwrap_or(true) {
                    *best = Some(m);
                }
            }
        }
    }
    let mut best = None;
    walk(dir, 0, &mut best);
    best
}

/// Liveness mtime for a conversation: newest write across its brain subtree AND
/// the SQLite store (WAL mode — `<id>.db-wal` is touched on every turn; the bare
/// `.db` only at checkpoint, so we stat all of `.db`/`.db-wal`/`.db-shm`).
/// transcript.jsonl is covered by the subtree walk but is NOT relied on — agy can
/// fail to write it entirely (observed: empty file + a path error in cli.log).
pub(crate) fn conv_liveness(conv_dir: &Path) -> Option<SystemTime> {
    let mut best = newest_mtime_under(conv_dir);
    if let Some(id) = conv_dir.file_name() {
        let db_base = conversations_dir().join(id);
        for ext in ["db", "db-wal", "db-shm"] {
            let p = db_base.with_extension(ext);
            if let Ok(m) = std::fs::metadata(&p).and_then(|md| md.modified()) {
                if best.map(|b| m > b).unwrap_or(true) {
                    best = Some(m);
                }
            }
        }
    }
    best
}

/// Cap on a background task's tail excerpt inside the folded answer
/// (docs/feedback-2026-09-17 P5): a domain command can exit non-zero as an
/// ordinary warning (not a crash) and still dump a `result.json` running to
/// thousands of lines, and the receipt only needs enough to tell success from
/// failure at a glance — not the payload.
const TASK_RESULT_TAIL_CHARS: usize = 160;

/// Whether a task result counts as ok for the purposes of a summary tally —
/// the same exit-code-beats-phrase priority `bg_finish_ok`/`format_task_result_line`
/// use, so "N ok, M failed" in a condensed summary agrees with the per-task
/// marks shown next to it.
fn task_result_ok(t: &TaskResult) -> bool {
    match t.exit_code {
        Some(0) => true,
        Some(_) => false,
        None => t.ok,
    }
}

/// One `✓/✗ <task-id>: ok|FAILED(exit=N) · <tail>` line for a single
/// background task result, with the tail excerpt clipped to
/// `TASK_RESULT_TAIL_CHARS`. Pure formatting, split out of
/// `append_task_results` so it and `condensed_task_result_lines` render a task
/// identically.
pub(crate) fn format_task_result_line(t: &TaskResult) -> String {
    let short = t.id.rsplit('/').next().unwrap_or(&t.id);
    let exit_str = match t.exit_code {
        Some(code) => format!(" (exit={code})"),
        None => String::new(),
    };
    let (mark, state) = if task_result_ok(t) {
        ("✓", "ok")
    } else {
        ("✗", "FAILED")
    };
    let tail_display = match &t.summary {
        Some(sum) => format!("{sum} · {}", t.tail),
        None => t.tail.clone(),
    };
    let tail_display = clip(&tail_display, TASK_RESULT_TAIL_CHARS);
    format!("{mark} {short}: {state}{exit_str} · {tail_display}")
}

/// Keep only the LAST result of each command: a sweep reruns the same build or test
/// until it passes, and an early `✗` that a later run fixed is not the outcome —
/// listing it sends the reader off to re-verify something already green. Results
/// with no known command stay as they are. Order follows each survivor's position.
pub(crate) fn dedupe_task_results(tasks: &[TaskResult]) -> Vec<TaskResult> {
    let key = |t: &TaskResult| t.cmd.as_deref().map(normalize_command);
    tasks
        .iter()
        .enumerate()
        .filter(|(i, t)| match key(t) {
            Some(k) => !tasks[i + 1..]
                .iter()
                .any(|u| key(u).as_deref() == Some(k.as_str())),
            None => true,
        })
        .map(|(_, t)| t.clone())
        .collect()
}

/// The task-result lines to print, after `dedupe_task_results`. One task: its full
/// line. Several and all ok: a single `bg: N tasks, final all ok` line. Otherwise
/// a line per failure (at most 5, then `… N more failed`) and one line counting the
/// ones that passed. A run with 20+ background results otherwise buries the answer
/// (docs/feedback-2026-09-17 P5, a 1-job run with 20 `✗` lines). Pure.
pub(crate) fn condensed_task_result_lines(tasks: &[TaskResult]) -> Vec<String> {
    let last = dedupe_task_results(tasks);
    if last.len() == 1 {
        return last.iter().map(format_task_result_line).collect();
    }
    let failed: Vec<&TaskResult> = last.iter().filter(|t| !task_result_ok(t)).collect();
    let ok = last.len() - failed.len();
    let runs = if tasks.len() > last.len() {
        format!(" ({} runs)", tasks.len())
    } else {
        String::new()
    };
    if failed.is_empty() {
        return vec![format!("bg: {} tasks{runs}, final all ok", last.len())];
    }
    let mut out: Vec<String> = failed
        .iter()
        .take(5)
        .map(|t| format_task_result_line(t))
        .collect();
    if failed.len() > 5 {
        out.push(format!("… {} more failed", failed.len() - 5));
    }
    if ok > 0 {
        out.push(format!("✓ {ok} other task(s) ok{runs}"));
    }
    out
}

/// Fold completed-background-task outcomes into the answer. agy's re-ignited turn
/// after a build is usually empty, so the bare answer is a "ビルド実行中…" waiting
/// line; appending the captured result makes the report reflect the build outcome.
pub(crate) fn append_task_results(answer: String, tasks: &[TaskResult]) -> String {
    if tasks.is_empty() {
        return answer;
    }
    let mut s = answer;
    if !s.is_empty() {
        s.push_str("\n\n");
    }
    s.push_str("— background task results —");
    for line in condensed_task_result_lines(tasks) {
        s.push('\n');
        s.push_str(&line);
    }
    s
}

/// How many times one normalized `run_command` line must repeat in a single
/// run before it's worth a footer warning (docs/feedback-2026-09-17 P2-2): a
/// 3605s run turned out, from the diff, to have swept 20+ parameter
/// combinations without ever isolating the actual cause — invisible in the
/// answer, which was just "waiting for completion" repeated. 8 is well above
/// an ordinary retry-once-on-failure pattern.
pub(crate) const REPEATED_COMMAND_THRESHOLD: usize = 8;

/// Collapse a command line for repeat-detection: trim and fold internal
/// whitespace runs to a single space. Two invocations differing only in
/// incidental spacing (e.g. an extra blank between flags) should still count
/// as "the same command" — this is a grouping key, not the display text (see
/// `repeated_command_warning`, which clips separately for display).
pub(crate) fn normalize_command(cmd: &str) -> String {
    cmd.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Footer warning when one normalized `run_command` line repeats at least
/// `REPEATED_COMMAND_THRESHOLD` times within a single run's own step trace —
/// usually a parameter sweep or a retry loop that never diagnosed the actual
/// cause (see `REPEATED_COMMAND_THRESHOLD`). Picks the most-repeated line
/// when several qualify, ties broken by first occurrence; None when nothing
/// crosses the threshold. `lines` is this run's own `run_command` text, in
/// order (`agy::run_command_lines`) — not the whole conversation's, so a long
/// resumed session doesn't accumulate a false positive across turns.
pub(crate) fn repeated_command_warning(lines: &[String]) -> Option<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for l in lines {
        let norm = normalize_command(l);
        if norm.is_empty() {
            continue;
        }
        match counts.iter_mut().find(|(c, _)| *c == norm) {
            Some((_, n)) => *n += 1,
            None => counts.push((norm, 1)),
        }
    }
    let (cmd, n) = counts.into_iter().max_by_key(|(_, n)| *n)?;
    if n < REPEATED_COMMAND_THRESHOLD {
        return None;
    }
    Some(format!("⚠ ran \"{}\" {n} times", clip(&cmd, 60)))
}

/// Extract the most actionable error line from captured stdout/stderr.
/// Without filtering cargo wrapper boilerplate ("could not compile ... due to N previous errors"),
/// the footer surfaces empty summaries instead of the actual compiler diagnostics or test assertions.
fn extract_error_snippet(stdout: &str, stderr: &str) -> Option<String> {
    let clean_err = strip_broken_encoding(stderr);
    let clean_out = strip_broken_encoding(stdout);

    // Stderr first (compiler diagnostics), then stdout.
    let mut lines: Vec<&str> = Vec::new();
    for l in clean_err.lines().chain(clean_out.lines()) {
        let trimmed = l.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines.push(trimmed);
    }

    if lines.is_empty() {
        return None;
    }

    // 1. Mangled output marker (Windows CP932 / non-UTF8 console error).
    // An unreadable console line must be flagged explicitly rather than guessed.
    for line in &lines {
        if line.starts_with("⟨") && line.ends_with("dropped: non-UTF-8 console output⟩") {
            return Some((*line).to_string());
        }
    }

    // 2. Direct compiler / syntax / type errors: error[E0425], error TS2322, etc.
    for line in &lines {
        if line.starts_with("error[E")
            || line.starts_with("error TS")
            || line.contains("error[E")
            || line.contains("error TS")
        {
            return Some((*line).to_string());
        }
    }

    // 3. Panics, assertions, and language-level exceptions.
    for line in &lines {
        if line.contains("panicked at")
            || line.contains("panic at")
            || (line.contains("assertion `") && line.contains("` failed"))
            || line.contains("assertion failed:")
            || line.contains("AssertionError:")
            || line.contains("TypeError:")
            || line.contains("ValueError:")
            || line.contains("SyntaxError:")
            || line.contains("ReferenceError:")
        {
            return Some((*line).to_string());
        }
    }

    // 4. Test runner explicit failures (pytest, vitest, jest, cargo test, go test).
    for line in &lines {
        if line.starts_with("FAILED ")
            || line.starts_with("FAIL ")
            || line.starts_with("--- FAIL:")
            || (line.starts_with("test ") && line.ends_with("... FAILED"))
        {
            return Some((*line).to_string());
        }
    }

    // 5. General compiler / tool error lines (skip cargo wrapper noise).
    for line in &lines {
        if (line.starts_with("error:") || line.starts_with("Error:"))
            && !line.contains("could not compile")
            && !line.contains("process didn't exit successfully")
        {
            return Some((*line).to_string());
        }
    }

    // 6. Test runner summary line if it reported failures (e.g. `test result: FAILED. 1 passed; 1 failed`).
    if let Some(sum) = find_test_summary(&clean_out).or_else(|| find_test_summary(&clean_err)) {
        let lower = sum.to_lowercase();
        if lower.contains("failed") {
            return Some(sum);
        }
    }

    // 7. Any line containing "error:" or "Error" or "FAILED" or "failed".
    for line in &lines {
        let lower = line.to_lowercase();
        if (lower.contains("error:") || lower.contains("failed"))
            && !lower.contains("could not compile")
            && !lower.contains("process didn't exit successfully")
        {
            return Some((*line).to_string());
        }
    }

    // 8. Fallback: last non-empty line.
    lines.last().map(|l| (*l).to_string())
}

/// Turn a verification command's captured output into ONE token-frugal line
/// for the report footer (`✓ cargo test: ok` or `✗ cargo test: FAILED (exit 101) · error[E0425]: ...`).
/// Without this, verification failures force the caller to open a full log file
/// just to see the first compiler or test assertion error, blowing up orchestrator context.
pub(crate) fn summarize_verify_output(
    cmd: &str,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
) -> String {
    let cmd_clean = clip(cmd.trim(), 40);
    let cmd_label = if cmd_clean.is_empty() {
        "verify"
    } else {
        &cmd_clean
    };

    if exit_code == 0 {
        let clean_out = strip_broken_encoding(stdout);
        let clean_err = strip_broken_encoding(stderr);
        let summary = find_test_summary(&clean_out).or_else(|| find_test_summary(&clean_err));
        match summary {
            Some(sum) => {
                let compact = clip(&sum, 60);
                format!("✓ {cmd_label}: ok ({compact})")
            }
            None => format!("✓ {cmd_label}: ok"),
        }
    } else {
        let err_snippet = extract_error_snippet(stdout, stderr);
        match err_snippet {
            Some(raw_err) => {
                let cleaned = clip(&raw_err, 90);
                format!("✗ {cmd_label}: FAILED (exit {exit_code}) · {cleaned}")
            }
            None => format!("✗ {cmd_label}: FAILED (exit {exit_code})"),
        }
    }
}

const DEFAULT_VERIFY_TIMEOUT_S: u64 = 120;

/// How long the supervisor-side check may take before it is abandoned. Not an MCP
/// arg: nobody is blocked on it (the run is already over and its answer is already
/// in hand), so the caller has no better information than this default.
fn verify_timeout_s() -> u64 {
    std::env::var("AGY_VERIFY_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_VERIFY_TIMEOUT_S)
}

/// Resolve a program name to a concrete executable path, given PATH and PATHEXT strings
/// and a filesystem existence check.
///
/// Pure so it can be tested deterministically without touching the real disk.
/// A name that already carries an extension, or that is an absolute or relative path,
/// resolves to itself.
/// Without this, bare Windows commands (.cmd / .bat shims like npm, yarn, pnpm, npx)
/// fail to execute under `CreateProcess`.
pub(crate) fn resolve_program<F>(
    prog: &str,
    path_var: Option<&str>,
    pathext_var: Option<&str>,
    exists: F,
) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    let prog_path = Path::new(prog);
    // If it is an explicit relative/absolute path or already has an extension,
    // it resolves to itself.
    let has_sep = prog.contains('/') || prog.contains('\\');
    let has_ext = prog_path.extension().is_some();
    if has_sep || has_ext {
        return Some(prog_path.to_path_buf());
    }

    let default_pathext = if cfg!(windows) { DEFAULT_PATHEXT } else { "" };
    let pathext = pathext_var.unwrap_or(default_pathext);
    let exts: Vec<&str> = if pathext.is_empty() {
        Vec::new()
    } else {
        pathext
            .split(';')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect()
    };

    let path = path_var.unwrap_or("");
    let path_sep = if cfg!(windows) { ';' } else { ':' };

    for dir in path
        .split(path_sep)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let base = Path::new(dir).join(prog);
        // On non-Windows or if PATHEXT is empty, try the bare name directly.
        if exts.is_empty() {
            if exists(&base) {
                return Some(base);
            }
        } else {
            for ext in &exts {
                let candidate = base.with_extension(ext.strip_prefix('.').unwrap_or(ext));
                if exists(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn parse_sdk_version(s: &str) -> Option<Vec<u64>> {
    if !s.contains('.') {
        return None;
    }
    s.split('.').map(|p| p.parse::<u64>().ok()).collect()
}

/// Image paths (`.png`/`.jpg`/`.jpeg`/`.webp`/`.gif`, case-insensitive) named in
/// `prompt`: absolute paths, or workspace-relative ones joined onto `ws`. Tokens
/// are split on whitespace and stripped of surrounding quotes/backticks/brackets
/// and trailing punctuation. De-duplicated, first-seen order. Pure.
pub(crate) fn image_paths_in_prompt(prompt: &str, ws: &Path) -> Vec<PathBuf> {
    fn strip_token(mut s: &str) -> &str {
        loop {
            let trimmed = s
                .trim_start_matches(['"', '\'', '`', '(', '[', '{', '<'])
                .trim_end_matches(['"', '\'', '`', ')', ']', '}', '>'])
                .trim_end_matches(['.', ',', ';', ':', '!', '?']);
            if trimmed == s {
                break;
            }
            s = trimmed;
        }
        s
    }

    fn is_abs(s: &str) -> bool {
        let bytes = s.as_bytes();
        if bytes.is_empty() {
            return false;
        }
        if bytes[0] == b'/' || bytes[0] == b'\\' {
            return true;
        }
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return true;
        }
        Path::new(s).is_absolute()
    }

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    for token in prompt.split_whitespace() {
        let clean = strip_token(token);
        if clean.is_empty() {
            continue;
        }
        let p = Path::new(clean);
        let is_image = match p.extension().and_then(|e| e.to_str()) {
            Some(ext) => matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif"
            ),
            None => false,
        };
        if !is_image {
            continue;
        }
        let resolved = if is_abs(clean) {
            PathBuf::from(clean)
        } else {
            ws.join(clean)
        };
        if seen.insert(resolved.clone()) {
            out.push(resolved);
        }
    }

    out
}

/// Is `bytes` a complete image of the kind its signature claims? Pure.
/// PNG: 8-byte signature and ends with the `IEND` chunk (`IEND` + 4-byte CRC as
/// the last 8 bytes). JPEG: starts `FF D8` and ends `FF D9` (trailing NUL/whitespace
/// padding tolerated). WEBP: `RIFF` + `WEBP` at 8..12 and the RIFF size field
/// (u32 LE at 4..8) + 8 <= len. GIF: `GIF87a`/`GIF89a` and ends with `0x3B`.
/// Anything else (unknown signature, empty) → false.
pub(crate) fn image_looks_complete(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }

    // PNG: 8-byte signature and ends with the `IEND` chunk (`IEND` + 4-byte CRC as
    // the last 8 bytes).
    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.starts_with(&PNG_SIG) {
        return bytes.len() >= 16 && &bytes[bytes.len() - 8..bytes.len() - 4] == b"IEND";
    }

    // JPEG: starts `FF D8` and ends `FF D9` (trailing NUL/whitespace padding tolerated).
    if bytes.starts_with(&[0xFF, 0xD8]) {
        let mut end = bytes.len();
        while end > 0 && matches!(bytes[end - 1], 0 | b' ' | b'\t' | b'\r' | b'\n') {
            end -= 1;
        }
        return end >= 2 && bytes[end - 2..end] == [0xFF, 0xD9];
    }

    // WEBP: `RIFF` + `WEBP` at 8..12 and the RIFF size field (u32 LE at 4..8) + 8 <= len.
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        let riff_size = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        return (riff_size as u64) + 8 <= bytes.len() as u64;
    }

    // GIF: `GIF87a`/`GIF89a` and ends with `0x3B`.
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return bytes.last() == Some(&0x3B);
    }

    false
}

/// `"image that may not decode: a.png, b.jpg"` for the prompt's image paths whose
/// bytes `read` returns but that fail [`image_looks_complete`], or None when there
/// are none. A path `read` cannot open is skipped (absent ≠ broken). Pure over `read`.
pub(crate) fn broken_image_note(
    prompt: &str,
    ws: &Path,
    read: impl Fn(&Path) -> Option<Vec<u8>>,
) -> Option<String> {
    let paths = image_paths_in_prompt(prompt, ws);
    let mut broken = Vec::new();
    for p in paths {
        if let Some(bytes) = read(&p) {
            if !image_looks_complete(&bytes) {
                broken.push(p);
            }
        }
    }
    if broken.is_empty() {
        None
    } else {
        let list = broken
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("image that may not decode: {list}"))
    }
}

/// The newest Windows SDK version among `names` (entries of
/// `Windows Kits\10\bin`, e.g. `10.0.22621.0`), compared numerically per dotted
/// part; non-version names (`x64`, `arm64`) are ignored. Pure.
pub(crate) fn newest_sdk_version(names: &[String]) -> Option<String> {
    names
        .iter()
        .filter_map(|name| {
            let ver = parse_sdk_version(name)?;
            Some((ver, name))
        })
        .max_by(|(v1, _), (v2, _)| v1.cmp(v2))
        .map(|(_, name)| name.clone())
}

/// `path_var` with `sdk_bin` appended (`;`-joined), or None when nothing needs
/// changing: `sdk_bin` is None, or `rc.exe` already resolves on `path_var`
/// (`rc_exists_on_path`). Pure.
pub(crate) fn path_with_sdk_bin(
    path_var: Option<&str>,
    sdk_bin: Option<&Path>,
    rc_exists_on_path: bool,
) -> Option<String> {
    if rc_exists_on_path {
        return None;
    }
    let sdk = sdk_bin?;
    let sdk_str = sdk.to_string_lossy();
    match path_var {
        Some(p) if !p.is_empty() => {
            let p_trimmed = p.strip_suffix(';').unwrap_or(p);
            Some(format!("{p_trimmed};{sdk_str}"))
        }
        _ => Some(sdk_str.into_owned()),
    }
}

/// `%ProgramFiles(x86)%\Windows Kits\10\bin\<newest>\x64` when it holds `rc.exe`.
/// A build script that shells out to `rc.exe` (`tauri-winres`, `embed-resource`)
/// panics under a client-started server, whose PATH lacks the SDK; a developer
/// prompt or VS build has it. Reads disk; None anywhere but a Windows box with
/// the SDK.
fn windows_sdk_bin() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let pf = std::env::var_os("ProgramFiles(x86)")?;
        let kits_bin = PathBuf::from(pf)
            .join("Windows Kits")
            .join("10")
            .join("bin");
        if !kits_bin.is_dir() {
            return None;
        }
        let names: Vec<String> = std::fs::read_dir(&kits_bin)
            .ok()?
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let newest = newest_sdk_version(&names)?;
        let candidate = kits_bin.join(newest).join("x64");
        if candidate.join("rc.exe").is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Run a preset's `verify` command in `ws` and summarize it for the footer.
///
/// Best-effort by construction: the run it follows has already succeeded, so a
/// check that cannot be spawned reports *itself* as the problem rather than
/// turning a good run into a failed one. The command was vetted at preset-resolve
/// time ([`crate::presets::verify_command_is_safe`]) and is a single program with
/// arguments — no shell is involved, which is why it can be tokenized on
/// whitespace here.
///
/// `preset_env` is the same (already substituted/validated) env the agy child for
/// this run got — a lane's `verify` must see the same `CARGO_TARGET_DIR` its build
/// used, or it would check the wrong tree. Applied before the bridge's own
/// `PATHEXT` fixup below, same ordering rule as the agy spawn.
pub(crate) async fn run_verify_command(
    cmd: &str,
    ws: &Path,
    preset_env: &BTreeMap<String, String>,
) -> String {
    let mut tokens = cmd.split_whitespace();
    let Some(prog) = tokens.next() else {
        return "⚠ verify: empty command".to_string();
    };

    let path_var = std::env::var("PATH").ok();
    #[cfg(windows)]
    let pathext_var = pathext_fixup(std::env::var("PATHEXT").ok().as_deref())
        .or_else(|| std::env::var("PATHEXT").ok());
    #[cfg(not(windows))]
    let pathext_var = std::env::var("PATHEXT").ok();

    let resolved = resolve_program(prog, path_var.as_deref(), pathext_var.as_deref(), |p| {
        p.is_file()
    });

    let Some(resolved_path) = resolved else {
        return format!("⚠ verify `{cmd}` could not run: program '{prog}' not found on PATH");
    };

    let is_windows_script = if cfg!(windows) {
        resolved_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
            .unwrap_or(false)
    } else {
        false
    };

    let mut c = if is_windows_script {
        // On Windows CreateProcess cannot execute a `.cmd` or `.bat` directly without `cmd /c`.
        // This is safe here because `cmd` already passed `presets::verify_command_is_safe`
        // -> `agy::is_safe_command_segment`, so it is a single segment with a restricted
        // argument character set, and it comes from a preset file rather than from the sub-agent.
        let mut cmd_proc = Command::new("cmd");
        cmd_proc.arg("/c").arg(&resolved_path).args(tokens);
        cmd_proc
    } else {
        let mut direct_proc = Command::new(&resolved_path);
        direct_proc.args(tokens);
        direct_proc
    };

    // Preset env first, same ordering rule as the agy spawn: the bridge's own
    // PATHEXT fixup right below is applied AFTER, so it always wins.
    for (k, v) in preset_env {
        c.env(k, v);
    }
    c.current_dir(ws)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
        // Same fixup as the agy spawn: a client-started MCP server inherits no
        // PATHEXT, and without it `cargo` does not resolve to `cargo.exe`.
        if let Some(ref v) = pathext_var {
            c.env("PATHEXT", v);
        }
        // Seed the SDK bin (rc.exe) when PATH lacks it — see `windows_sdk_bin`.
        let rc_exists_on_path =
            resolve_program("rc", path_var.as_deref(), pathext_var.as_deref(), |p| {
                p.is_file()
            })
            .is_some();
        let sdk_bin = windows_sdk_bin();
        if let Some(p) =
            path_with_sdk_bin(path_var.as_deref(), sdk_bin.as_deref(), rc_exists_on_path)
        {
            c.env("PATH", p);
        }
    }
    let secs = verify_timeout_s();
    match tokio::time::timeout(Duration::from_secs(secs), c.output()).await {
        Ok(Ok(o)) => summarize_verify_output(
            cmd,
            o.status.code().unwrap_or(-1),
            &String::from_utf8_lossy(&o.stdout),
            &String::from_utf8_lossy(&o.stderr),
        ),
        Ok(Err(e)) => format!("⚠ verify `{cmd}` could not run: {e}"),
        Err(_) => format!("⚠ verify `{cmd}` timed out after {secs}s"),
    }
}

/// Names of the shells among a tree's process image names. agy's MCP servers are
/// always in the tree (node, zenith, …), so only shells say "a command is running".
pub(crate) fn shell_names(procs: &[String]) -> Vec<String> {
    const SHELLS: &[&str] = &["powershell", "pwsh", "cmd", "bash", "sh", "wsl"];
    let mut out: Vec<String> = Vec::new();
    for p in procs {
        let lower = p.to_lowercase();
        let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
        if SHELLS.contains(&stem) && !out.iter().any(|o| o == stem) {
            out.push(stem.to_string());
        }
    }
    out
}

/// One line saying what the run looked like when the bridge stopped it: the last
/// step, whether a background task was still unfinished, whether a shell was still
/// alive. This is what tells a hung agy from one waiting on a long command (j5,
/// 2026-10-19: the route to the idle stop could not be reconstructed afterwards).
pub(crate) fn stop_snapshot(
    reason: StopReason,
    silent_s: u64,
    last_step: Option<&str>,
    pending_bg: Option<usize>,
    procs: &[String],
) -> String {
    let kind = match reason {
        StopReason::Idle => "idle",
        StopReason::HardCap => "hard cap",
        StopReason::QuotaWall(_) => "quota wall",
    };
    let bg = match pending_bg {
        Some(0) => "no unfinished background task".to_string(),
        Some(n) => format!("{n} unfinished background task(s)"),
        None => "background tasks unknown".to_string(),
    };
    let shells = shell_names(procs);
    let sh = if shells.is_empty() {
        "no shell descendant".to_string()
    } else {
        format!("shell alive: {}", shells.join(","))
    };
    format!(
        "stopped ({kind}, silent {silent_s}s) · last step: {} · {bg} · {sh}",
        last_step.unwrap_or("none")
    )
}

/// Append the stop snapshot to `~/.agy-mcp/stops.jsonl` (not runs.jsonl: stats
/// counts every line there as a run). Best-effort.
fn log_stop(job_id: Option<&str>, note: &str) {
    let entry = serde_json::json!({ "t": now_unix(), "job": job_id.unwrap_or(""), "note": note });
    let _ = std::fs::create_dir_all(crate::agy::paths::agy_mcp_home());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::agy::paths::agy_mcp_home().join("stops.jsonl"))
    {
        use std::io::Write;
        let _ = writeln!(f, "{entry}");
    }
}

/// Stop agy and everything it started: the job object where there is one, plus
/// agy.exe itself (the whole of the old behavior where there is none).
fn stop_tree(child: &mut tokio::process::Child, tree: Option<&crate::agy::jobobj::ProcessTree>) {
    if let Some(t) = tree {
        t.terminate();
    }
    let _ = child.start_kill();
}

/// Tags agy itself is known to flush into `-p` stdout when a resumed conversation
/// carries pending background-task notifications (observed live, 2026-07-11 probe).
/// Only these are stripped — a generic any-tag strip would eat XML/HTML snippets
/// that legitimately appear in coding answers.
pub(crate) const SYSTEM_BLOCK_TAGS: &[&str] = &[
    "task-notification",
    "USER_SETTINGS_CHANGE",
    "stdout",
    "stderr",
    "log",
];

/// What the run got done before it died, from the transcript we already parsed.
/// A failure that reports only its last denial reads as "nothing happened" —
/// observed 2026-07-27: four successful commands and a full `cargo test` behind
/// a permission denial, all of it already on disk. Pure so it is unit-tested.
pub(crate) fn progress_note(t: Option<&Transcript>) -> Option<String> {
    let t = t?;
    if t.step_summary.is_empty() && t.artifacts.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !t.step_summary.is_empty() {
        parts.push(t.step_summary.clone());
    }
    if !t.artifacts.is_empty() {
        let shown: Vec<String> = t
            .artifacts
            .iter()
            .take(5)
            .map(|p| {
                Path::new(p)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.clone())
            })
            .collect();
        let mut s = format!("wrote: {}", shown.join(", "));
        if t.artifacts.len() > 5 {
            s.push_str(&format!(" (+{} more)", t.artifacts.len() - 5));
        }
        parts.push(s);
    }
    let mut note = format!("before the failure: {}", parts.join(" · "));
    if let Some(last_err) = t.errors.last() {
        note.push_str(&format!(
            " · error: {}",
            crate::agy::transcript::clip(last_err, 120)
        ));
    }
    Some(note)
}

/// A bridge stop (Idle/HardCap) that left steps but no answer, as a cut-off Ok
/// outcome instead of an error, so `run_core`'s one-shot "summarize what's
/// unfinished" resume runs for it too (D8: j77 and j21 returned a bare FAILED
/// after 19 and 76 steps). `None` — the caller keeps the plain stop error — when
/// the stop forces failure (`stop_forces_failure`), there is no `conv_id` to
/// resume, or `partial` shows no work (`progress_note` is `None`).
/// The text is `reason.partial_note()`, a blank line, `stop_message(..)`, a
/// newline and the progress note; `bare_answer` is empty; `via` is
/// "transcript"; the remaining fields come from `partial` (ctx_tokens from
/// `session_ctx_tokens(conv)` when `conv` is Some, else 0); `cut_off` is true.
pub(crate) fn stopped_without_answer(
    reason: StopReason,
    partial: Option<&Transcript>,
    conv: Option<&Path>,
    conv_id: Option<String>,
    idle_timeout_s: u64,
    timeout_s: u64,
) -> Option<AgyOutcome> {
    if stop_forces_failure(reason) {
        return None;
    }
    let conv_id = conv_id?;
    let t = partial?;
    let note = progress_note(Some(t))?;
    let text = format!(
        "{}\n\n{}\n{note}",
        reason.partial_note(),
        stop_message(reason, idle_timeout_s, timeout_s)
    );
    Some(AgyOutcome {
        text,
        bare_answer: String::new(),
        conv_id: Some(conv_id),
        via: "transcript",
        served_model: t.served_model.clone(),
        artifacts: t.artifacts.clone(),
        step_summary: t.step_summary.clone(),
        ctx_tokens: conv.map(session_ctx_tokens).unwrap_or(0),
        task_results: t.task_results.clone(),
        cut_off: true,
    })
}

// Each argument is an independent input to ONE decision; splitting them into structs would
// scatter the decision that exists here precisely so it is made in a single place.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_outcome(
    stopped: Option<StopReason>,
    stdout_raw: &[u8],
    stderr_raw: &[u8],
    exit_err: Option<anyhow::Error>,
    conv: Option<PathBuf>,
    conv_id: Option<String>,
    transcript_res: Option<Result<Transcript, anyhow::Error>>,
    idle_timeout_s: u64,
    timeout_s: u64,
) -> Result<AgyOutcome> {
    // A transcript with no answer still carries the steps the run took — which is
    // exactly the failure where saying what got done matters most.
    let partial = match transcript_res.as_ref() {
        Some(Ok(t)) => Some(t),
        Some(Err(e)) => e
            .downcast_ref::<crate::agy::transcript::NoAnswerError>()
            .map(|n| &n.partial),
        None => None,
    };
    let note = progress_note(partial);
    let stream_cut = agy_stream_interrupt_note(&String::from_utf8_lossy(stderr_raw));
    let name_cut = |err: anyhow::Error| -> anyhow::Error {
        match stream_cut {
            Some(n) => err.context(n),
            None => err,
        }
    };
    let attach_note = |err: anyhow::Error| -> anyhow::Error {
        if let Some(ref note_str) = note {
            err.context(note_str.clone())
        } else {
            err
        }
    };

    let stdout_str = String::from_utf8_lossy(stdout_raw);
    let clean_stdout = strip_system_blocks(&stdout_str).trim().to_string();
    // A failed run (exit 3: partway through a reply, then a model/agent error) can leave
    // what agy had already said on stdout while the transcript has no answer. Fold it
    // into the failure instead of dropping it, the way a quota stop keeps its partial.
    let keep_stdout = |err: anyhow::Error| -> anyhow::Error {
        let partial = strip_broken_encoding(&clean_stdout);
        if partial.is_empty() {
            err
        } else {
            err.context(format!("partial answer: {partial}"))
        }
    };
    // agy's own failure receipt, when the run ended on an error after answering.
    let receipt_note = exit_err.as_ref().and_then(|_| {
        parse_agy_error(&String::from_utf8_lossy(stderr_raw)).map(|l| {
            format!(
                "⚠ agy reported an error after this response (AGY_ERROR {l}) — the answer below may be partial"
            )
        })
    });

    let has_timeout_msg =
        String::from_utf8_lossy(stderr_raw).contains("timeout waiting for response");

    // stdout is the first answer candidate only when it is clean
    let is_stdout_clean =
        stopped.is_none() && exit_err.is_none() && !has_timeout_msg && !clean_stdout.is_empty();

    if is_stdout_clean {
        let (served_model, artifacts, step_summary, task_results, ctx_tokens) =
            if let Some(Ok(t)) = &transcript_res {
                let ctx_tokens = conv.as_ref().map(|c| session_ctx_tokens(c)).unwrap_or(0);
                (
                    t.served_model.clone(),
                    t.artifacts.clone(),
                    t.step_summary.clone(),
                    t.task_results.clone(),
                    ctx_tokens,
                )
            } else {
                (None, Vec::new(), String::new(), Vec::new(), 0)
            };

        let bare_answer = strip_broken_encoding(&clean_stdout);
        let text = strip_broken_encoding(&append_task_results(clean_stdout, &task_results));
        let truncation_note = agy_truncation_note(&String::from_utf8_lossy(stderr_raw));
        let text = match truncation_note {
            Some(note) => format!("{note}\n\n{}", text.trim()),
            None => text,
        };
        return Ok(AgyOutcome {
            text,
            bare_answer,
            conv_id,
            via: "stdout",
            served_model,
            artifacts,
            step_summary,
            ctx_tokens,
            task_results,
            cut_off: is_cut_off(stopped, truncation_note.is_some()),
        });
    }

    // (2) transcript fallback — used if stdout is empty (old versions, regression, or no stdout).
    let stopped_from_ok = stopped.and_then(|reason| {
        let ok_partial = match transcript_res.as_ref() {
            Some(Ok(t)) => Some(t),
            _ => None,
        };
        stopped_without_answer(
            reason,
            ok_partial,
            conv.as_deref(),
            conv_id.clone(),
            idle_timeout_s,
            timeout_s,
        )
    });

    if let Some(res) = transcript_res {
        match res {
            Ok(t) => {
                if !t.answer.is_empty() {
                    let ctx_tokens = conv.as_ref().map(|c| session_ctx_tokens(c)).unwrap_or(0);
                    let bare_answer = strip_broken_encoding(&t.answer);
                    let text =
                        strip_broken_encoding(&append_task_results(t.answer, &t.task_results));

                    // A quota wall is a failure regardless of whether agy had already
                    // written a partial answer before hitting it — see
                    // `stop_forces_failure`. stop_message's QuotaWall arm already
                    // carries the QUOTA_EXHAUSTED:/RESOURCE_EXHAUSTED (code 429)
                    // wording `record_quota_wall` depends on; the partial answer
                    // survives as context instead of becoming the reported text, and
                    // `attach_note` still folds in the step-tally/`wrote:`/last-error
                    // progress note the same as any other failure.
                    if let Some(reason) = stopped {
                        if stop_forces_failure(reason) {
                            let mut err = anyhow!(stop_message(reason, idle_timeout_s, timeout_s));
                            let trimmed = text.trim();
                            if !trimmed.is_empty() {
                                err = err.context(format!("partial answer: {trimmed}"));
                            }
                            return Err(attach_note(err));
                        }
                    }

                    // We got here with `stopped` set only because WE killed agy and it
                    // had already written something. Say so — see `partial_note`.
                    // Two independent reasons the answer may be cut off: the bridge
                    // stopped agy, or agy stopped itself at `--print-timeout`. Either
                    // one has to be said out loud, or a partial reads as a success.
                    let truncation_note = agy_truncation_note(&String::from_utf8_lossy(stderr_raw));
                    let cut = stopped
                        .map(|r| r.partial_note())
                        .or_else(|| truncation_note.or(stream_cut).map(str::to_string))
                        .or(receipt_note);
                    let text = match cut {
                        Some(note) => format!("{note}\n\n{}", text.trim()),
                        None => text,
                    };
                    return Ok(AgyOutcome {
                        text,
                        bare_answer,
                        conv_id,
                        via: "transcript",
                        served_model: t.served_model,
                        artifacts: t.artifacts,
                        step_summary: t.step_summary,
                        ctx_tokens,
                        task_results: t.task_results,
                        cut_off: is_cut_off(stopped, truncation_note.is_some()),
                    });
                }
            }
            Err(e) => {
                if let Some(err) = exit_err {
                    return Err(attach_note(name_cut(keep_stdout(err))));
                }
                if let Some(reason) = stopped {
                    let partial = e
                        .downcast_ref::<crate::agy::transcript::NoAnswerError>()
                        .map(|n| &n.partial);
                    if let Some(outcome) = stopped_without_answer(
                        reason,
                        partial,
                        conv.as_deref(),
                        conv_id.clone(),
                        idle_timeout_s,
                        timeout_s,
                    ) {
                        return Ok(outcome);
                    }
                    return Err(attach_note(anyhow!(stop_message(
                        reason,
                        idle_timeout_s,
                        timeout_s
                    ))));
                }
                // agy 1.1.3+ soft-denies un-granted tools in headless mode. Trigger on
                // the bare "auto-denied" marker (the wording around it can drift) and
                // treat the parsed kind as a bonus — an unparsed kind still yields the
                // old, useful error instead of falling through to trust/OAuth guidance.
                let stderr_str = String::from_utf8_lossy(stderr_raw);
                if stderr_str.contains("auto-denied") {
                    let kind = parse_denied_permission(&stderr_str).unwrap_or_default();
                    let (tool, target) = conv
                        .as_ref()
                        .and_then(|c| read_last_tool_call(c))
                        .unwrap_or_default();
                    return Err(attach_note(anyhow::Error::new(DeniedError {
                        kind,
                        tool,
                        target,
                        conv_id,
                    })));
                }
                return Err(attach_note(e));
            }
        }
    }

    if let Some(err) = exit_err {
        return Err(attach_note(name_cut(keep_stdout(err))));
    }

    // Disambiguate timeout when no answer was found and no transcript error occurred.
    if let Some(reason) = stopped {
        if let Some(outcome) = stopped_from_ok {
            return Ok(outcome);
        }
        return Err(attach_note(anyhow!(stop_message(
            reason,
            idle_timeout_s,
            timeout_s
        ))));
    }

    Err(attach_note(anyhow!(
        "agy produced no stdout and no usable transcript was found under {}",
        brain_dir().display()
    )))
}

/// How long to keep reading agy's stdout/stderr after agy itself has exited.
const PIPE_CLOSE_GRACE_S: u64 = 5;

/// Reads one of agy's output pipes into a shared buffer, so the bytes read so far
/// survive giving up on it.
///
/// **Joining the reader unbounded after agy exits can hang the run forever.** A child
/// agy started (a daemon background command) inherits the pipe, so EOF waits on *it*,
/// not on agy — and the heartbeat loop has already ended, so no idle cap fires. agy
/// 1.2.9's changelog names exactly this ("headless runs leaving daemon background
/// processes running after exit, which could hang scripts reading the CLI's output
/// until end-of-file"); it also fits the 2026-07-26 job that sat "running" for 29
/// minutes with its transcript dead at +23s (design-notes). Bounding the join holds
/// whatever agy upstream does, and a killed agy's children are not killed with it.
struct PipeReader {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    task: tokio::task::JoinHandle<()>,
}

impl PipeReader {
    fn spawn<R>(pipe: Option<R>) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        use tokio::io::AsyncReadExt;
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = buf.clone();
        let task = tokio::spawn(async move {
            let Some(mut p) = pipe else { return };
            let mut chunk = [0u8; 8192];
            while let Ok(n) = p.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(&chunk[..n]);
            }
        });
        Self { buf, task }
    }

    /// Everything read, and whether the pipe was still open when `grace` ran out.
    async fn finish(mut self, grace: Duration) -> (Vec<u8>, bool) {
        let still_open = tokio::time::timeout(grace, &mut self.task).await.is_err();
        if still_open {
            self.task.abort();
        }
        let bytes = std::mem::take(&mut *self.buf.lock().unwrap_or_else(|e| e.into_inner()));
        (bytes, still_open)
    }
}

/// After agy exits, a background task (e.g. `cargo build` it WAITs on) may still
/// be running; its completion is appended to the transcript later. Keep holding
/// the job: re-read the transcript until every pending task reports completion,
/// or the same idle/hard caps fire. agy's process is already gone, so this only
/// watches files (no child to reap). Cheap first check avoids any wait on the
/// common no-pending-task path.
async fn drain_pending_tasks(
    conv_dir: &Path,
    since: Option<i64>,
    live_conv: &Option<PathBuf>,
    run_start: SystemTime,
    idle_cap: Duration,
    hard_cap: Duration,
) {
    if read_full_since(conv_dir, since)
        .map(|t| t.pending_tasks.is_empty())
        .unwrap_or(true)
    {
        return;
    }
    let mut last_mtime = live_conv.as_deref().and_then(conv_liveness);
    let mut last_progress = SystemTime::now();
    loop {
        tokio::time::sleep(Duration::from_secs(HEARTBEAT_TICK_S)).await;
        if read_full_since(conv_dir, since)
            .map(|t| t.pending_tasks.is_empty())
            .unwrap_or(true)
        {
            return;
        }
        let m = live_conv.as_deref().and_then(conv_liveness);
        if m > last_mtime {
            last_mtime = m;
            last_progress = SystemTime::now();
        }
        if wall_since(last_progress) > idle_cap || wall_since(run_start) > hard_cap {
            return; // genuinely stalled or hit the absolute ceiling — give up waiting
        }
    }
}

/// Windows' own default list, used when the inherited `PATHEXT` can't resolve a
/// plain `cargo`.
#[cfg(windows)]
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC";

/// What `PATHEXT` the agy child should get, or None to inherit unchanged.
///
/// **This is the fix for the chronic "shell can't find cargo".** The MCP server is
/// started by the client with **no `PATHEXT` at all**, and **PowerShell appends `.CPL`
/// to whatever it inherits** — so with nothing to append to, the shell's whole list
/// becomes `.CPL`, and it refuses to resolve `cargo` to `cargo.exe` while
/// `where.exe cargo` (its own extension logic) still finds it. That contradiction is
/// exactly what sent sub-agents hunting for absolute paths and `powershell -Command`
/// wrappers.
///
/// **The `.CPL` is PowerShell's, not agy's** (re-measured 2026-08-15, and the earlier
/// note here blamed agy): `pwsh -NoProfile` and `powershell.exe` both turn a parent's
/// `.COM;.EXE` into `.COM;.EXE;.CPL`, and an absent one into `.CPL`. agy only passes its
/// environment down. So repairing it here is not a workaround for an agy bug — it is
/// this bridge supplying the variable its own launcher failed to pass on.
///
/// Missing `.EXE` is the whole test: a list that has it already is the user's own and is
/// left alone.
#[cfg(windows)]
fn pathext_fixup(current: Option<&str>) -> Option<String> {
    let cur = current.unwrap_or("").trim();
    let has_exe = cur
        .split(';')
        .any(|e| e.trim().eq_ignore_ascii_case(".exe"));
    if has_exe {
        return None;
    }
    // Keep whatever was there — it may carry a genuinely wanted extension — but put a
    // usable list in front of it.
    Some(if cur.is_empty() {
        DEFAULT_PATHEXT.to_string()
    } else {
        format!("{DEFAULT_PATHEXT};{cur}")
    })
}

/// Wait (bounded by [`CONV_DIR_DETECT_S`]) for a RESUMED conversation to show new
/// activity — the resume-side equivalent of a fresh run's dir appearing. Used only
/// to decide when a scoped-MCP run may put the shared config back; a `false`
/// return just means we keep the swap until the run ends.
async fn await_liveness_advance(
    dir: Option<&Path>,
    before: Option<SystemTime>,
    child: &mut tokio::process::Child,
) -> bool {
    let Some(dir) = dir else { return false };
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_secs(CONV_DIR_DETECT_S) {
        if conv_liveness(dir) > before {
            return true;
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true; // agy is done; it certainly read the config
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Run `agy -p` once and return the model's answer.
/// `since` is the job's pre-spawn step baseline (see `run_agy`): every read of
/// what the run did — quota wall, pending tasks, answer, failure summary — covers
/// only the steps past it.
async fn run_agy_once(opts: &RunOpts<'_>, since: Option<i64>) -> Result<AgyOutcome> {
    let bin = agy_bin();

    // agy takes the prompt as an argv element, and Windows CreateProcess caps the
    // WHOLE command line at 32767 chars — oversize fails with an opaque OS spawn
    // error ("ファイル名…の構文が間違っています"). The prompt dominates, but long /
    // many add_dirs (+ model + conversation id + exe path) also count, so budget the
    // assembled length, not the prompt alone. ~30K leaves headroom under the cap.
    const MAX_CMDLINE: usize = 30_000;
    let extra: usize = opts.model.map(|m| m.len() + 10).unwrap_or(0)
        + opts.add_dirs.iter().map(|d| d.len() + 12).sum::<usize>()
        + opts.continue_id.map(|c| c.len() + 16).unwrap_or(0)
        + opts.workspace.as_os_str().len() * 2 // cwd + the `--add-dir <workspace>`
        + bin.len()
        + 52;
    let total = opts.prompt.chars().count() + extra;
    if total > MAX_CMDLINE {
        anyhow::bail!(
            "command line too long (~{total} chars incl. prompt + paths; agy's CLI invocation \
             caps ~{MAX_CMDLINE}). Put the bulk in a file, add its folder via add_dirs, and name \
             the file in the prompt."
        );
    }

    // Ensure workspaces are trusted so agy doesn't stall on the prompt in headless mode.
    let mut workspaces_to_trust = vec![opts.workspace];
    let mut add_dir_paths = Vec::new();

    for d in opts.add_dirs {
        let d = d.trim();
        if !d.is_empty() {
            let p = Path::new(d);
            add_dir_paths.push(p);
        }
    }

    workspaces_to_trust.extend(&add_dir_paths);

    if let Err(e) = ensure_workspaces_trusted(&workspaces_to_trust) {
        eprintln!("Warning: failed to trust workspaces: {}", e);
    }

    // agy 1.1.3+ headless soft-denies any tool needing permission confirmation
    // (observed: `soft-denying tool confirmation "Edit"` killing a coding run;
    // `--mode accept-edits` does NOT help — print mode stays request-review).
    // The sanctioned fix is a `write_file(<dir>)` allow-rule in settings.json
    // `permissions.allow` (named by agy's own stderr notice), which
    // ensure_workspaces_trusted merges alongside the trust entry above.
    let mut cmd = Command::new(&bin);
    cmd.arg("-p")
        .arg(opts.prompt)
        .arg("--print-timeout")
        .arg(format!("{}s", opts.timeout_s));
    // The dispatched preset's own env (e.g. a per-lane CARGO_TARGET_DIR), applied
    // FIRST. Everything below is the bridge's own — applied AFTER so it always
    // wins on a key collision, even if a validator bug ever let one through; see
    // `RunOpts::preset_env`.
    for (k, v) in opts.preset_env {
        cmd.env(k, v);
    }
    // Mark this run delegated for the PreToolUse guard, and hand it whatever policy
    // the dispatching agent chose. agy passes its environment to hook processes, so
    // this is what separates "a run nobody is watching" from a person at the
    // keyboard — the guard leaves the latter alone.
    cmd.env(crate::hookguard::DELEGATED_ENV, "1");
    // A delegated run must not update agy in place. Print mode runs agy's own updater,
    // and on 2026-09-11 it replaced the binary two seconds into a job (1.1.28 → 1.2.1);
    // every run after that died on a behaviour 1.1.28 never had. The operator's
    // interactive agy still updates, so this only moves WHEN an upgrade lands.
    // The value must be `true`: with `1` agy spawned its updater anyway, with `true` it
    // logged `Auto-update disabled via environment variable` (measured 2026-09-13).
    cmd.env("AGY_CLI_DISABLE_AUTO_UPDATE", "true");
    // An agy-mcp that agy starts under this run sees the mark and exits (crate::nest).
    cmd.envs(crate::nest::spawned_env());
    // An `agy-mcp <subcommand>` the run shells out to resolves this workspace.
    cmd.env("AGY_WORKSPACE", opts.workspace);
    // agy's run_command gives git a terminal, so a long `git grep` / `git log` opened
    // `less` and waited for a key nobody presses: j77 (2026-10-20) sat 223s on
    // `git grep pending_edit` with a cmd shell alive until the idle stop killed it.
    // No pager and no credential prompt for anything agy shells out.
    for (k, v) in NON_INTERACTIVE_ENV {
        cmd.env(k, v);
    }
    match &opts.guard_policy {
        Some(json) => {
            cmd.env(crate::hookguard::POLICY_ENV, json);
        }
        // Never inherit a stale one from the server's own environment.
        None => {
            cmd.env_remove(crate::hookguard::POLICY_ENV);
        }
    }
    if let Some(m) = opts.model {
        cmd.arg("--model").arg(wire_model(m));
    }
    if opts.continue_id.is_none() {
        if let Some(name) = opts.agent {
            if let Err(e) = crate::agy::agents::ensure_builtin_agent(name) {
                eprintln!("Warning: failed to ensure builtin agent `{name}`: {e}");
            }
            cmd.arg("--agent").arg(name);
        }
    }
    for d in opts.add_dirs {
        if !d.trim().is_empty() {
            cmd.arg("--add-dir").arg(d);
        }
    }
    // Grant the workspace itself as a workspace dir (not just user add_dirs). agy's
    // tool-runner otherwise defaults to its own scratch even though cwd is set to
    // the workspace; pairing this with the absolute-path directive in the prompt
    // (run_core) is what makes agy actually create/edit files in the workspace.
    cmd.arg("--add-dir").arg(opts.workspace);
    if let Some(id) = opts.continue_id {
        cmd.arg("--conversation").arg(id);
    }
    // Setup-time opt-in (`-e AGY_PERMISSIONS=trusted` on the `claude mcp add` line),
    // never a tool arg: whether this machine's sub-agents run un-gated is the human
    // operator's standing decision, not something a caller picks per task. Default
    // is the gate + the pre-granted families + soft-deny recovery.
    if crate::agy::permissions::permission_mode() == PermissionMode::Trusted {
        cmd.arg("--dangerously-skip-permissions");
    }
    cmd.current_dir(opts.workspace)
        .stdin(Stdio::null()) // <-- THE fix: open stdin freezes agy at startup
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true); // timeout => future dropped => child killed

    // No console window. We're launched from a windowless MCP server, so agy itself
    // has no console; when its autonomous agent shells out (e.g. `cargo check`), the
    // child would otherwise pop a fresh console window. CREATE_NO_WINDOW keeps the
    // whole process tree windowless. (ConPTY scrapes in pty.rs use a separate spawn.)
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW); // tokio::process::Command inherent (Windows)

        // agy hands its own PATHEXT to the shell it spawns, and an MCP server started
        // by the client has none — leaving the sub-agent unable to run `cargo`.
        if let Some(v) = pathext_fixup(std::env::var("PATHEXT").ok().as_deref()) {
            cmd.env("PATHEXT", v);
        }
    }

    // Spawn, and for a FRESH run detect its new conversation dir, under the spawn
    // gate. Holding the gate across spawn + detection makes concurrent fresh runs
    // each claim their OWN new dir (set-difference vs a pre-spawn snapshot) instead
    // of racing other runs. Resumes know their dir, so they skip the gate.
    let is_fresh = opts.continue_id.is_none();
    // The pre-spawn snapshot outlives the gate on purpose. Detection below is
    // bounded so the gate can't stall every other fresh spawn, but a cold start
    // that overruns it must not cost us the conversation: the heartbeat retries
    // the SAME set-difference, which is what keeps attribution honest (we can
    // only ever adopt a dir that did not exist before we spawned).
    let mut before: Option<std::collections::HashSet<String>> = None;
    // Pre-spawn liveness of the conversation being resumed: a resume has no new
    // dir to watch for, so "agy is up" is this advancing.
    let resume_dir = opts.continue_id.map(|id| brain_dir().join(id));
    let pre_liveness = resume_dir.as_deref().and_then(conv_liveness);
    // Restored early (below) once agy is past its config read; the guard itself
    // stays alive as the backstop for every path that doesn't get that far.
    let mut mcp_guard;
    let tree;
    let (mut child, mut fresh_conv) = {
        // Read side for an ordinary run, write side for a scoped one. Readers do
        // not contend, so the common path pays only the spawn itself.
        let (_swap_r, _swap_w) = match (&opts.mcp, opts.swap_gate) {
            (_, None) => (None, None),
            (McpMode::Inherit, Some(l)) => (Some(l.read().await), None),
            (_, Some(l)) => {
                let w = l.write().await;
                // Holding the write lock stops NEW spawns, but an agy spawned just
                // before we got it may not have read the config yet. Let it.
                tokio::time::sleep(Duration::from_secs(SWAP_SETTLE_S)).await;
                (None, Some(w))
            }
        };
        mcp_guard = apply_mcp_swap(&opts.mcp).await?;
        let _gate = match (is_fresh, opts.spawn_gate) {
            (true, Some(m)) => Some(m.lock().await),
            _ => None,
        };
        if is_fresh {
            before = Some(brain_dir_names());
        }
        let mut child = cmd.spawn().with_context(|| {
            format!("spawning agy ({bin}); set AGY_BIN to its full path if not found")
        })?;
        tree = crate::agy::jobobj::ProcessTree::of_child(&child);
        let fresh_conv = match &before {
            Some(before) => {
                let t = std::time::Instant::now();
                let mut found = None;
                while t.elapsed() < Duration::from_secs(CONV_DIR_DETECT_S) {
                    if let Some(d) = newest_dir_not_in(before) {
                        found = Some(d);
                        break;
                    }
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        found = newest_dir_not_in(before); // agy exited; last look
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                found
            }
            None => None,
        };
        // Give the config back as soon as agy has demonstrably read it, instead of
        // at the end of the run. Signal: the conversation dir appearing (fresh) or
        // its liveness advancing (resume) — both are agy writing, which only
        // happens well past startup. Measured 2026-08-14: restoring on the dir
        // signal at 4.72s and 4.95s kept the scope clean, while a FIXED 5.0s leaked
        // the full server list on a slower run. The signal tracks initialisation;
        // a clock does not. If no signal arrives in time, we simply don't restore
        // early — the guard falls back to end-of-run, which is the old behaviour.
        if mcp_guard.is_some() {
            let started = if is_fresh {
                fresh_conv.is_some()
            } else {
                await_liveness_advance(resume_dir.as_deref(), pre_liveness, &mut child).await
            };
            if started {
                if let Some(g) = mcp_guard.as_mut() {
                    g.restore();
                }
            }
        }
        (child, fresh_conv)
    };

    // Liveness-based ("heartbeat") timeout. agy's only timeout is `--print-timeout`,
    // a fixed TOTAL wall-clock that also counts time agy spends WAITING on a build it
    // shelled out — so a long task that's still making progress gets cut off mid-build
    // (observed: a storage-layer conversion died at the 600s budget while a `cargo
    // build` it had launched as a background task was still RUNNING). Instead of
    // letting a fixed total govern, we kill only when agy goes IDLE (no conversation-
    // store / brain-dir writes) for `idle_timeout_s`, bounded by the generous
    // `timeout_s` hard ceiling. agy still gets `--print-timeout = timeout_s` as a
    // backstop. Liveness via conv_liveness (SQLite WAL + brain subtree).
    // Mutable: a fresh run whose dir hadn't appeared by CONV_DIR_DETECT_S can still
    // acquire one from the heartbeat below. Until it does there is no liveness to
    // read, which `should_stop` treats as "unknown", not "idle".
    let mut live_conv: Option<PathBuf> = match opts.continue_id {
        Some(id) => Some(brain_dir().join(id)),
        None => fresh_conv.clone(),
    };
    // Drain stdout/stderr concurrently so a full pipe buffer can't block agy while we
    // poll (agy's stdout is empty per #76, but stderr can carry warnings).
    let stdout_pipe = PipeReader::spawn(child.stdout.take());
    let stderr_pipe = PipeReader::spawn(child.stderr.take());

    // Record the conversation on the job mirror the moment we know it: from here
    // on, even a job whose server dies is RESUMABLE rather than merely re-runnable.
    if let (Some(jid), Some(dir)) = (opts.job_id, live_conv.as_deref()) {
        let cid = dir.file_name().map(|s| s.to_string_lossy().into_owned());
        note_job_progress(jid, cid.as_deref(), Some(now_unix()), None);
    }

    let hard_cap = Duration::from_secs(opts.timeout_s);
    let idle_cap = Duration::from_secs(opts.idle_timeout_s.max(30));
    // Caps are measured on the WALL clock, not `Instant`. A monotonic clock can
    // stop across a machine suspend, which froze the idle timer on a run that was
    // already dead (observed 2026-07-26: agy silent from +23s, the job kept
    // "running" for 29 more minutes and died with the server). Wall time cannot
    // be paused, so a stalled run is reaped even if the box slept through it.
    let run_start = SystemTime::now();
    let mut last_progress = run_start;
    let mut last_mtime = live_conv.as_deref().and_then(conv_liveness);
    let mut last_mirror = SystemTime::now();
    let mut last_ping = SystemTime::now();
    let mut ping_n: u32 = 0;
    let mut stopped: Option<StopReason> = None;
    let status = loop {
        tokio::select! {
            r = child.wait() => break r?,
            _ = tokio::time::sleep(Duration::from_secs(HEARTBEAT_TICK_S)) => {
                let now = SystemTime::now();
                // Late conv-dir attribution. Still the set-difference against OUR
                // pre-spawn snapshot, so we can only adopt a dir that did not exist
                // when we spawned — the gate's guarantee is not weakened, only its
                // deadline. Without this a slow cold start left the run with no
                // liveness source for its whole life.
                if live_conv.is_none() {
                    if let Some(d) = before.as_ref().and_then(newest_dir_not_in) {
                        // The idle clock starts when we can first observe activity,
                        // not at spawn: charging a run for the time we were blind
                        // would kill it on the very next tick.
                        last_progress = now;
                        last_mtime = conv_liveness(&d);
                        if let Some(jid) = opts.job_id {
                            let cid = d.file_name().map(|s| s.to_string_lossy().into_owned());
                            note_job_progress(jid, cid.as_deref(), Some(unix_of(now)), None);
                        }
                        fresh_conv = Some(d.clone());
                        live_conv = Some(d);
                    }
                }
                let m = live_conv.as_deref().and_then(conv_liveness);
                if m > last_mtime {
                    last_mtime = m;
                    last_progress = now;
                    // On a tick where liveness advanced, check if agy is trapped in a quota
                    // retry loop (RESOURCE_EXHAUSTED 429). Without this, each retry entry advances
                    // mtime and resets idle progress, preventing idle timeout from firing.
                    if let Some(dir) = &live_conv {
                        if let Some(wall) = quota_wall_stop(dir, since) {
                            stopped = Some(wall);
                            stop_tree(&mut child, tree.as_ref());
                        }
                    }
                }
                // Mirror liveness periodically (not every tick — this is a
                // read-modify-write on a shared file) so `agy_poll` / `agy-mcp
                // wait` can say "running 1780s but SILENT 1757s" instead of
                // reporting healthy progress on a corpse. The same tick also
                // reads the transcript for the current tool call, so a
                // supervisor running several jobs can tell what each is doing
                // right now instead of only that it's "running Ns" — without
                // this a bare resume's `prompt_head` is the ONLY clue, and it's
                // always the same generic preamble text (see `with_preamble`).
                if let Some(jid) = opts.job_id {
                    if wall_since(last_mirror) >= Duration::from_secs(JOB_MIRROR_TICK_S) {
                        last_mirror = now;
                        let last_step = live_conv
                            .as_deref()
                            .and_then(|d| read_last_step(d, now_unix()));
                        note_job_progress(
                            jid,
                            None,
                            Some(unix_of(last_progress)),
                            last_step.as_deref(),
                        );
                    }
                }
                // Tell the waiting caller the run is alive. Silence here is what the
                // client's idle timer counts, and the session is blocked on this call.
                if let Some(sink) = &opts.progress {
                    if wall_since(last_ping) >= Duration::from_secs(PROGRESS_TICK_S) {
                        last_ping = now;
                        ping_n += 1;
                        sink(
                            ping_n,
                            progress_line(
                                wall_since(run_start).as_secs(),
                                live_conv.as_ref().map(|_| wall_since(last_progress).as_secs()),
                            ),
                        );
                    }
                }
                // select! drops the wait() future before running this arm, so child
                // is free to borrow; start_kill signals, next loop's wait() reaps it.
                if stopped.is_none() {
                    stopped = should_stop(
                        live_conv.as_ref().map(|_| wall_since(last_progress)),
                        wall_since(run_start),
                        idle_cap,
                        hard_cap,
                    );
                    if let Some(reason) = stopped {
                        // Before the kill: afterwards the tree is gone and the
                        // evidence with it.
                        let shells: Vec<String> = tree
                            .as_ref()
                            .map(|t| t.process_names())
                            .unwrap_or_default();
                        let pending = live_conv
                            .as_deref()
                            .and_then(|d| crate::agy::transcript::read_full(d).ok())
                            .map(|t| t.pending_tasks.len());
                        let last_step = live_conv
                            .as_deref()
                            .and_then(|d| read_last_step(d, now_unix()));
                        let note = stop_snapshot(
                            reason,
                            wall_since(last_progress).as_secs(),
                            last_step.as_deref(),
                            pending,
                            &shells,
                        );
                        eprintln!("[agy-mcp] {note}");
                        log_stop(opts.job_id, &note);
                        if let Some(jid) = opts.job_id {
                            note_job_progress(jid, None, None, Some(&note));
                        }
                        stop_tree(&mut child, tree.as_ref());
                    }
                }
            }
        }
    };
    // A run agy ended itself leaves its background tasks to the resume path; only a
    // run WE stopped (or one whose future is dropped) takes its tree down with it.
    if stopped.is_none() {
        if let Some(t) = &tree {
            t.disarm();
        }
    }
    let grace = Duration::from_secs(PIPE_CLOSE_GRACE_S);
    let (stdout, out_open) = stdout_pipe.finish(grace).await;
    let (stderr, err_open) = stderr_pipe.finish(grace).await;
    if out_open || err_open {
        eprintln!(
            "[agy-mcp] agy exited but its output pipe stayed open (a leftover child holds it); \
             stopped reading after {PIPE_CLOSE_GRACE_S}s"
        );
    }
    let output = WaitOutput {
        status,
        stdout,
        stderr,
    };

    let is_non_zero_exit = stopped.is_none() && !output.status.success();
    let exit_err = if is_non_zero_exit {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // With agy's own one-line receipt the cause leads and the raw tail shrinks to
        // context; without one the tail is all there is.
        let receipt = parse_agy_error(&stderr);
        let keep = if receipt.is_some() { 300 } else { 800 };
        let tail: String = stderr
            .chars()
            .rev()
            .take(keep)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        // A bare exit with no receipt is how agy dies on an image it can't decode
        // (docs/multimodal-notes.md §3); name the culprit when the prompt points at one.
        let image_note = if receipt.is_none() {
            broken_image_note(opts.prompt, opts.workspace, |p| std::fs::read(p).ok())
        } else {
            None
        };
        let image_note = image_note.map(|n| format!("\n{n}")).unwrap_or_default();
        Some(match receipt {
            Some(line) => anyhow!(
                "agy exited with {:?}: AGY_ERROR {line}\nstderr: {tail}",
                output.status.code()
            ),
            None => anyhow!(
                "agy exited with {:?}{image_note}\nstderr: {}",
                output.status.code(),
                tail
            ),
        })
    } else {
        None
    };

    // Locate the conversation dir. A *resumed* run appends to the existing dir
    // (known id). A FRESH run uses the dir detected under the spawn gate above.
    // If diff detection failed, we return a clear error instead of falling back to
    // timestamp scanning.
    // One last look before giving up: agy may have created the dir after our
    // bounded detection window AND after the final heartbeat tick. Throwing away a
    // completed run because we stopped looking is the worst outcome available here.
    if fresh_conv.is_none() {
        fresh_conv = before.as_ref().and_then(newest_dir_not_in);
    }
    let conv = match opts.continue_id {
        Some(id) => brain_dir().join(id),
        None => match fresh_conv {
            Some(d) => d,
            None => {
                // agy never created the conv dir. If it exited non-zero, THAT is the
                // real cause — surface its exit code/stderr instead of a generic message.
                if let Some(err) = exit_err {
                    return Err(err.context(
                        "agy exited before creating a conversation directory (diff detection failed)",
                    ));
                }
                anyhow::bail!(
                    "Failed to detect newly created conversation directory (diff detection failed)."
                );
            }
        },
    };
    let conv_id = conv.file_name().map(|s| s.to_string_lossy().into_owned());

    // If agy ended its turn with a background task (e.g. `cargo build`) still
    // pending, the agy process has exited but the task finishes out-of-band and
    // appends its completion to the transcript. Keep holding the job until that
    // lands (bounded by the same idle/hard caps) so the answer reflects the build
    // outcome instead of a bare "ビルド実行中…" waiting line. Skip if WE stopped it.
    if stopped.is_none() {
        drain_pending_tasks(&conv, since, &live_conv, run_start, idle_cap, hard_cap).await;
    }

    // Try reading transcript for rich metadata and fallback answer.
    let transcript_res = Some(read_full_since(&conv, since));

    resolve_outcome(
        stopped,
        &output.stdout,
        &output.stderr,
        exit_err,
        Some(conv),
        conv_id,
        transcript_res,
        opts.idle_timeout_s,
        opts.timeout_s,
    )
}

/// Default number of permission grants one run may auto-recover from.
const AUTO_GRANT_BUDGET: usize = 4;

/// How many soft-deny recoveries a single run gets (`AGY_AUTO_GRANT_BUDGET`,
/// 0 disables the mechanism entirely).
fn auto_grant_budget() -> usize {
    std::env::var("AGY_AUTO_GRANT_BUDGET")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(AUTO_GRANT_BUDGET)
}

/// Run agy, auto-recovering from headless permission denials up to a budget.
///
/// agy can't prompt in `-p` mode, so a tool needing confirmation is soft-denied and
/// the run dies with no answer. When the denied target is safe to grant
/// (`auto_grant_rule` — build/test-family commands, or file access inside the run's
/// own workspace) the bridge grants it and resumes the SAME conversation, since
/// grants only take effect on the next spawn. Anything else surfaces as `DeniedError`
/// naming the exact target, so the decision stays with the caller.
///
/// **Why a budget and not a single retry.** Grants are exact-string matches, and an
/// agent naturally issues a *sequence* of distinct commands — so one denial is
/// rarely the only one. Observed 2026-07-26: `cargo search jaq` was denied, granted,
/// resumed and succeeded, and the very next step (`cargo new jaq_test`, also inside
/// the safe family) was denied with the retry budget already spent, killing the run.
///
/// Termination is guaranteed by three independent conditions: the budget, the
/// grantability test (`auto_grant_rule` returning None ends it), and the
/// **no-progress rule** — a rule that gets denied twice means granting it did not
/// take effect, so retrying it again would loop forever.
pub async fn run_agy(opts: RunOpts<'_>) -> Result<AgyOutcome> {
    // A resume's transcript still holds every earlier turn — after a quota wall,
    // that turn's trailing 429s. What this job reports must be only the steps
    // written from here on (`steps_after`). Taken once, before the first spawn, so
    // an auto-grant resume below still reports the attempt it continues.
    let since = opts
        .continue_id
        .and_then(|id| step_baseline(&brain_dir().join(id)));
    let budget = auto_grant_budget();
    let mut granted: Vec<String> = Vec::new();
    // Owned backing store for the resume opts; the loop rebinds these each round.
    let mut resume_prompt = String::new();
    let mut resume_conv = String::new();

    loop {
        let attempt = if granted.is_empty() {
            RunOpts {
                prompt: opts.prompt,
                workspace: opts.workspace,
                model: opts.model,
                add_dirs: opts.add_dirs,
                continue_id: opts.continue_id,
                timeout_s: opts.timeout_s,
                idle_timeout_s: opts.idle_timeout_s,
                mcp: opts.mcp.clone(),
                spawn_gate: opts.spawn_gate,
                swap_gate: opts.swap_gate,
                job_id: opts.job_id,
                progress: opts.progress.clone(),
                guard_policy: opts.guard_policy.clone(),
                preset_env: opts.preset_env,
                agent: opts.agent,
            }
        } else {
            RunOpts {
                prompt: &resume_prompt,
                workspace: opts.workspace,
                model: opts.model,
                add_dirs: opts.add_dirs,
                continue_id: Some(&resume_conv),
                timeout_s: opts.timeout_s,
                idle_timeout_s: opts.idle_timeout_s,
                mcp: opts.mcp.clone(),
                spawn_gate: opts.spawn_gate,
                swap_gate: opts.swap_gate,
                job_id: opts.job_id,
                progress: opts.progress.clone(),
                guard_policy: opts.guard_policy.clone(),
                preset_env: opts.preset_env,
                agent: None,
            }
        };

        let err = match run_agy_once(&attempt, since).await {
            Ok(mut outcome) => {
                if !granted.is_empty() {
                    outcome.text = format!(
                        "↻ auto-granted {} after headless permission denial(s); resumed.\n\n{}",
                        granted.join(", "),
                        outcome.text
                    );
                }
                return Ok(outcome);
            }
            // Once we've retried, the LATEST outcome is the current truth —
            // re-reporting the original denial would misdiagnose the run.
            Err(e) => e,
        };

        let Some(denied) = err.downcast_ref::<DeniedError>() else {
            return Err(err);
        };
        let (Some(conv_id), Some(rule)) = (
            denied.conv_id.clone(),
            auto_grant_rule(&denied.kind, &denied.target, opts.workspace),
        ) else {
            return Err(err);
        };
        if granted.contains(&rule) {
            // Granting it did not change agy's answer — stop rather than spin.
            return Err(err.context(format!(
                "{rule} was granted but agy denied it again — the allow-rule is not taking effect; \
                 grant it manually or run that step yourself"
            )));
        }
        if granted.len() >= budget {
            return Err(err.context(format!(
                "auto-grant budget exhausted after {} grant(s) ({}) — raise AGY_AUTO_GRANT_BUDGET, \
                 pre-grant the commands this task needs, or scope the task so it shells out less",
                granted.len(),
                granted.join(", ")
            )));
        }
        if !grant_permission_entry(&rule) {
            return Err(err);
        }
        resume_prompt =
            format!("Permission {rule} has been granted. Continue the task from where it stopped.");
        resume_conv = conv_id;
        granted.push(rule);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Duration = Duration::from_secs(180);
    const HARD: Duration = Duration::from_secs(3600);

    #[test]
    fn spawned_agy_never_gets_a_pager_or_a_git_prompt() {
        let env: std::collections::HashMap<_, _> = NON_INTERACTIVE_ENV.into_iter().collect();
        assert_eq!(env.get("GIT_PAGER"), Some(&"cat"));
        assert_eq!(env.get("PAGER"), Some(&"cat"));
        assert_eq!(env.get("GIT_TERMINAL_PROMPT"), Some(&"0"));
    }

    #[test]
    fn a_run_with_no_liveness_source_is_never_idle() {
        // The bug this pins: a fresh run whose conversation dir appeared after the
        // detection window had NOTHING to read liveness from, so `last_progress`
        // never advanced and the run was killed at the idle cap — while working
        // perfectly — and then reported as a stall. "No signal" is not "no progress".
        assert_eq!(
            should_stop(None, Duration::from_secs(600), IDLE, HARD),
            None
        );
        // The hard cap is the only thing that still bounds it.
        assert_eq!(
            should_stop(None, Duration::from_secs(3601), IDLE, HARD),
            Some(StopReason::HardCap)
        );
    }

    #[test]
    fn a_located_conversation_that_goes_quiet_is_still_idle() {
        assert_eq!(
            should_stop(
                Some(Duration::from_secs(181)),
                Duration::from_secs(200),
                IDLE,
                HARD
            ),
            Some(StopReason::Idle)
        );
        assert_eq!(
            should_stop(
                Some(Duration::from_secs(179)),
                Duration::from_secs(200),
                IDLE,
                HARD
            ),
            None
        );
    }

    #[test]
    fn idle_beats_the_hard_cap_when_both_are_past() {
        // A run that is both stalled and over the ceiling is more usefully reported
        // stalled: "it hung" is actionable, "it ran an hour" is not.
        assert_eq!(
            should_stop(
                Some(Duration::from_secs(600)),
                Duration::from_secs(3601),
                IDLE,
                HARD
            ),
            Some(StopReason::Idle)
        );
    }

    #[cfg(windows)]
    #[test]
    fn pathext_is_repaired_only_when_exe_is_missing() {
        // The observed break: the MCP server inherits no PATHEXT, agy appends `.CPL`,
        // and the sub-agent's shell can no longer resolve `cargo` to `cargo.exe`.
        assert_eq!(pathext_fixup(None).as_deref(), Some(DEFAULT_PATHEXT));
        assert_eq!(pathext_fixup(Some("  ")).as_deref(), Some(DEFAULT_PATHEXT));
        assert_eq!(
            pathext_fixup(Some(".CPL")).as_deref(),
            Some(format!("{DEFAULT_PATHEXT};.CPL").as_str()),
            "an unusable list is prefixed, not discarded"
        );
        // A list that can already resolve an .exe is the user's own — leave it alone.
        assert!(pathext_fixup(Some(DEFAULT_PATHEXT)).is_none());
        assert!(
            pathext_fixup(Some(".com;.exe")).is_none(),
            "case-insensitive"
        );
        assert!(
            pathext_fixup(Some(".COM; .EXE ;.BAT")).is_none(),
            "spaces are not entries"
        );
    }

    /// A pipe a leftover child keeps open must not hold the run: the reader gives up
    /// after the grace period and still returns what agy wrote before exiting.
    #[tokio::test]
    async fn a_pipe_held_open_by_a_leftover_child_does_not_hang_the_run() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(64);
        let pipe = PipeReader::spawn(Some(reader));
        writer.write_all(b"final answer").await.unwrap();
        let (bytes, still_open) = pipe.finish(Duration::from_millis(200)).await;
        assert!(still_open);
        assert_eq!(bytes, b"final answer");
        drop(writer);

        let (mut writer, reader) = tokio::io::duplex(64);
        let pipe = PipeReader::spawn(Some(reader));
        writer.write_all(b"done").await.unwrap();
        drop(writer);
        assert_eq!(
            pipe.finish(Duration::from_secs(5)).await,
            (b"done".to_vec(), false)
        );
    }

    /// The unit test above proves the LOOKUP; this proves the SPAWN. `.cmd` is the
    /// whole point — `CreateProcess` cannot execute one, so before this every
    /// npm/yarn/pnpm verify came back "program not found" and the preset feature was
    /// silently Rust-only. Skipped where npm is not installed; CI's windows runner
    /// has it.
    #[tokio::test]
    async fn a_verify_command_that_is_a_cmd_shim_actually_runs() {
        let pathext = if cfg!(windows) {
            crate::agy::run::pathext_fixup(std::env::var("PATHEXT").ok().as_deref())
                .or_else(|| std::env::var("PATHEXT").ok())
        } else {
            None
        };
        let found = resolve_program(
            "npm",
            std::env::var("PATH").ok().as_deref(),
            pathext.as_deref(),
            |p| p.is_file(),
        );
        let Some(found) = found else {
            eprintln!("skipped: npm not on PATH");
            return;
        };
        let out =
            run_verify_command("npm --version", &std::env::temp_dir(), &BTreeMap::new()).await;
        assert!(
            !out.contains("not found"),
            "resolved {found:?} but the spawn still failed: {out}"
        );
        assert!(
            !out.contains("could not run"),
            "resolved {found:?} but the spawn still failed: {out}"
        );
    }

    /// agy 1.1.28 returns partial output with exit 0 when `--print-timeout` expires,
    /// so the ONLY signal that an answer was cut off is this stderr line. Measured
    /// verbatim on 2026-09-09.
    #[test]
    fn agys_own_print_timeout_is_recognized_as_a_truncation() {
        let real = "[agy] print timeout after 10s with turn in progress; returning partial output";
        assert!(agy_truncation_note(real).is_some());
        assert!(agy_truncation_note("").is_none());
        assert!(agy_truncation_note("warning: unused variable").is_none());
    }

    #[test]
    fn resolve_program_finds_cmd_and_exe_via_pathext_and_handles_missing_or_direct_paths() {
        let path_env = "C:\\tools;C:\\bin";
        let pathext_env = ".COM;.EXE;.BAT;.CMD";

        let fake_fs = |p: &Path| -> bool {
            let s = p.to_string_lossy();
            s.eq_ignore_ascii_case("C:\\tools\\npm.cmd")
                || s.eq_ignore_ascii_case("C:\\bin\\cargo.exe")
                || s.eq_ignore_ascii_case("C:\\bin\\direct.exe")
                || s.eq_ignore_ascii_case("relative\\path\\tool.exe")
        };

        // Bare name found via PATHEXT as .cmd
        assert_eq!(
            resolve_program("npm", Some(path_env), Some(pathext_env), fake_fs),
            Some(PathBuf::from("C:\\tools\\npm.CMD"))
        );

        // Same bare name found as .exe
        assert_eq!(
            resolve_program("cargo", Some(path_env), Some(pathext_env), fake_fs),
            Some(PathBuf::from("C:\\bin\\cargo.EXE"))
        );

        // Name found nowhere
        assert_eq!(
            resolve_program("nonexistent", Some(path_env), Some(pathext_env), fake_fs),
            None
        );

        // Name that already carries an extension resolves to itself
        assert_eq!(
            resolve_program(
                "C:\\bin\\direct.exe",
                Some(path_env),
                Some(pathext_env),
                fake_fs
            ),
            Some(PathBuf::from("C:\\bin\\direct.exe"))
        );

        // Relative path resolves to itself
        assert_eq!(
            resolve_program(
                "relative\\path\\tool.exe",
                Some(path_env),
                Some(pathext_env),
                fake_fs
            ),
            Some(PathBuf::from("relative\\path\\tool.exe"))
        );

        // Name that carries an extension resolves to itself even without path separators
        assert_eq!(
            resolve_program("mytool.exe", Some(path_env), Some(pathext_env), fake_fs),
            Some(PathBuf::from("mytool.exe"))
        );
    }

    #[test]
    fn newest_sdk_version_compares_numerically() {
        let names = vec![
            "10.0.9999.0".to_string(),
            "10.0.22621.0".to_string(),
            "x64".to_string(),
            "arm64".to_string(),
        ];
        assert_eq!(newest_sdk_version(&names), Some("10.0.22621.0".to_string()));
        assert_eq!(newest_sdk_version(&["x64".to_string()]), None);
        assert_eq!(newest_sdk_version(&[]), None);
    }

    #[test]
    fn path_with_sdk_bin_appends_only_when_rc_missing() {
        let sdk = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64");
        // rc already exists on PATH -> None
        assert_eq!(path_with_sdk_bin(Some(r"C:\bin"), Some(sdk), true), None);
        // sdk_bin is None -> None
        assert_eq!(path_with_sdk_bin(Some(r"C:\bin"), None, false), None);
        // rc missing on PATH -> appends sdk_bin
        assert_eq!(
            path_with_sdk_bin(Some(r"C:\bin"), Some(sdk), false),
            Some(format!(r"C:\bin;{}", sdk.display()))
        );
        // path_var is None -> returns sdk_bin
        assert_eq!(
            path_with_sdk_bin(None, Some(sdk), false),
            Some(sdk.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn newest_mtime_tracks_the_latest_write_recursively() {
        let base = std::env::temp_dir().join(format!("agy-live-{}", std::process::id()));
        let sub = base.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(base.join("old.txt"), b"1").unwrap();
        let m0 = newest_mtime_under(&base).expect("one file → some mtime");
        // A later nested write must move the newest mtime forward (recursion works).
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(sub.join("new.txt"), b"2").unwrap();
        let m1 = newest_mtime_under(&base).expect("two files → some mtime");
        assert!(
            m1 >= m0,
            "newest mtime should not go backwards after a nested write"
        );
        // A missing dir yields None (used to mean "no liveness yet").
        assert!(newest_mtime_under(&base.join("does-not-exist")).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_stop_message_names_the_disk_not_just_the_stop() {
        // Every variant has to say the same two things: what stopped it, and that
        // the edits are already on disk. A stop that reads as "nothing happened"
        // sends the caller re-running work that is already done.
        let idle = stop_message(StopReason::Idle, 180, 3600);
        assert!(
            idle.contains("idle timeout") && idle.contains("AGY_IDLE_TIMEOUT"),
            "{idle}"
        );
        let hard = stop_message(StopReason::HardCap, 180, 3600);
        assert!(
            hard.contains("hard cap") && hard.contains("AGY_TIMEOUT"),
            "{hard}"
        );
        let quota = stop_message(StopReason::QuotaWall(3), 180, 3600);
        assert!(quota.starts_with("QUOTA_EXHAUSTED:"), "{quota}");
        assert!(quota.contains("RESOURCE_EXHAUSTED (code 429)"), "{quota}");
        assert!(quota.contains("3 retries"), "{quota}");
        assert!(crate::agy::quota::is_quota_wall(&quota), "{quota}");

        for r in [
            StopReason::Idle,
            StopReason::HardCap,
            StopReason::QuotaWall(3),
        ] {
            assert!(stop_message(r, 180, 3600).contains("git diff"), "{r:?}");
            assert!(r.partial_note().starts_with('⚠'), "{r:?}");
        }
    }

    /// 2026-09-26 (j62 → j63): a Flash turn died on three 429s; resuming it on Opus
    /// was killed at the first heartbeat, before Opus was ever called, because the
    /// conversation's tail — the PREVIOUS turn's 429s — read as this run's retries.
    /// The failure text then carried the 429 wording `record_quota_wall` turns into
    /// a 0% receipt for the new model's group. Only the resume's own steps count.
    #[test]
    fn a_resume_after_a_quota_wall_is_not_stopped_by_the_previous_turns_429s() {
        let dir = std::env::temp_dir().join(format!("agy_test_resume_wall_{}", std::process::id()));
        let logs = dir.join(".system_generated").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let transcript = logs.join("transcript.jsonl");
        let wall = |i: u32, n: u32| {
            format!(
                r#"{{"step_index":{i},"source":"SYSTEM","type":"ERROR_MESSAGE","status":"DONE","error":"API error (attempt {n}): RESOURCE_EXHAUSTED (code 429): Resource has been exhausted."}}"#
            )
        };
        let prev = [
            r#"{"step_index":0,"source":"USER","type":"USER_INPUT","status":"DONE","content":"<USER_SETTINGS_CHANGE>The user changed setting `Model Selection` from None to Gemini 3.5 Flash (Medium). No need to comment on this change if the user doesn't ask about it.</USER_SETTINGS_CHANGE>"}"#.to_string(),
            r#"{"step_index":124,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"partial answer","tool_calls":[{"name":"run_command","args":{}}]}"#.to_string(),
            wall(125, 1),
            wall(126, 2),
            wall(127, 3),
        ]
        .join("\n")
            + "\n";
        std::fs::write(&transcript, &prev).unwrap();

        // Pre-spawn baseline; then agy forwards the message — liveness advances,
        // but no step has been appended to the transcript yet.
        let since = step_baseline(&dir);
        assert_eq!(since, Some(127));
        // The old read (whole conversation) is exactly the false stop.
        assert!(matches!(
            quota_wall_stop(&dir, None),
            Some(StopReason::QuotaWall(3))
        ));
        assert!(quota_wall_stop(&dir, since).is_none());

        // Post-run: no answer of its own, and nothing a 429 receipt could be read from.
        let err = resolve_outcome(
            None,
            b"",
            b"",
            None,
            Some(dir.clone()),
            Some("conv".into()),
            Some(read_full_since(&dir, since)),
            180,
            3600,
        )
        .err()
        .expect("the resume produced nothing of its own");
        let text = format!("{err:#}");
        assert!(!crate::agy::quota::is_quota_wall(&text), "{text}");
        assert!(!text.contains("partial answer"), "{text}");
        assert!(text.contains("added no steps"), "{text}");

        // A wall the resume hits ITSELF still stops it.
        let mut own = prev.clone();
        own.push_str(
            r#"{"step_index":128,"source":"USER","type":"USER_INPUT","status":"DONE","content":"continue"}"#,
        );
        own.push('\n');
        for (i, n) in [(129, 1), (130, 2), (131, 3)] {
            own.push_str(&wall(i, n));
            own.push('\n');
        }
        std::fs::write(&transcript, &own).unwrap();
        assert!(matches!(
            quota_wall_stop(&dir, since),
            Some(StopReason::QuotaWall(3))
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_should_stop_quota() {
        assert!(!should_stop_quota(0, 3));
        assert!(!should_stop_quota(2, 3));
        assert!(should_stop_quota(3, 3));
        assert!(should_stop_quota(4, 3));
        assert!(!should_stop_quota(5, 0));
    }

    #[test]
    fn only_quota_wall_forces_a_partial_answer_to_be_a_failure() {
        assert!(stop_forces_failure(StopReason::QuotaWall(3)));
        assert!(!stop_forces_failure(StopReason::Idle));
        assert!(!stop_forces_failure(StopReason::HardCap));
    }

    #[test]
    fn cut_off_is_truncation_or_bridge_idle_hard_cap_never_quota_wall() {
        assert!(is_cut_off(None, true), "agy's own print-timeout");
        assert!(is_cut_off(Some(StopReason::Idle), false));
        assert!(is_cut_off(Some(StopReason::HardCap), false));
        assert!(!is_cut_off(None, false), "a normal finished run");
        // QuotaWall never reaches `is_cut_off`'s Ok path in practice (it's
        // diverted to an Err by `stop_forces_failure` first), but the function
        // itself must not call it a cut-off reachable via the Ok branch either.
        assert!(!is_cut_off(Some(StopReason::QuotaWall(3)), false));
    }

    /// The bug this pins (observed 2026-09-17, j14/j15): a quota wall that had
    /// already produced a partial answer came back `Ok` with a `⚠ stopped by the
    /// bridge (quota wall)` prefix, so `agy-mcp wait` exited 0 and a caller
    /// checking only the exit code saw success. It must now be an Err carrying
    /// the QUOTA_EXHAUSTED:/RESOURCE_EXHAUSTED (code 429) wording
    /// `record_quota_wall`/`is_quota_wall` depend on, with the partial answer
    /// preserved as context rather than discarded.
    #[test]
    fn a_quota_wall_with_a_partial_answer_is_a_failure_not_a_done_job() {
        let t = Transcript {
            answer: "I got partway through: renamed the field in 3 files.".to_string(),
            served_model: None,
            artifacts: vec!["src/a.rs".into()],
            step_summary: "5 steps: 5×run_command".to_string(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 3,
        };
        let err = resolve_outcome(
            Some(StopReason::QuotaWall(3)),
            b"",
            b"",
            None,
            Some(PathBuf::from("conv-wall")),
            Some("conv-wall".to_string()),
            Some(Ok(t)),
            180,
            3600,
        )
        .err()
        .expect("a quota-wall stop with a partial answer must be an Err");
        let text = format!("{err:#}");
        assert!(text.contains("QUOTA_EXHAUSTED:"), "{text}");
        assert!(text.contains("RESOURCE_EXHAUSTED (code 429)"), "{text}");
        assert!(
            text.contains("renamed the field in 3 files"),
            "partial answer must survive: {text}"
        );
        assert!(crate::agy::quota::is_quota_wall(&text), "{text}");
    }

    /// Idle/hard-cap stops are NOT forced to fail: the bridge's own scheduling
    /// call on a run that may otherwise have kept working is not evidence the
    /// run itself failed, and the partial answer it already wrote is real,
    /// useful output — this must keep returning `Ok` (job status `done`).
    #[test]
    fn an_idle_stop_with_a_partial_answer_stays_a_done_job() {
        let t = Transcript {
            answer: "half a report".to_string(),
            served_model: None,
            artifacts: vec![],
            step_summary: "3 steps".to_string(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };
        let out = resolve_outcome(
            Some(StopReason::Idle),
            b"",
            b"",
            None,
            Some(PathBuf::from("conv-idle")),
            Some("conv-idle".to_string()),
            Some(Ok(t)),
            180,
            3600,
        )
        .expect("an idle stop with a partial answer stays Ok");
        assert!(out
            .text
            .starts_with("⚠ stopped by the bridge (idle timeout)"));
        assert!(out.text.ends_with("half a report"));
        assert!(out.cut_off, "idle stop with a partial answer is cut off");
    }

    #[test]
    fn stopped_without_answer_behavior() {
        let steps_t = Transcript {
            answer: String::new(),
            served_model: None,
            artifacts: vec![],
            step_summary: "6 steps: 4×run_command".to_string(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };

        // (a) Idle + conv_id + a partial transcript with a non-empty `step_summary` → Some,
        // `cut_off` true, text starts with the idle partial note and contains "idle timeout"
        let outcome = stopped_without_answer(
            StopReason::Idle,
            Some(&steps_t),
            None,
            Some("conv-123".to_string()),
            180,
            3600,
        );
        assert!(outcome.is_some());
        let out = outcome.unwrap();
        assert!(out.cut_off);
        assert!(
            out.text.starts_with(&StopReason::Idle.partial_note()),
            "{}",
            out.text
        );
        assert!(out.text.contains("idle timeout"), "{}", out.text);

        // (b) QuotaWall(3) → None
        assert!(stopped_without_answer(
            StopReason::QuotaWall(3),
            Some(&steps_t),
            None,
            Some("conv-123".to_string()),
            180,
            3600,
        )
        .is_none());

        // (c) conv_id None → None
        assert!(
            stopped_without_answer(StopReason::Idle, Some(&steps_t), None, None, 180, 3600,)
                .is_none()
        );

        // (d) a partial with empty step_summary and no artifacts → None
        let empty_t = Transcript {
            answer: String::new(),
            served_model: None,
            artifacts: vec![],
            step_summary: String::new(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };
        assert!(stopped_without_answer(
            StopReason::Idle,
            Some(&empty_t),
            None,
            Some("conv-123".to_string()),
            180,
            3600,
        )
        .is_none());
    }

    #[test]
    fn resolve_outcome_stopped_without_answer_recovers_as_cut_off() {
        let partial = Transcript {
            answer: String::new(),
            served_model: None,
            artifacts: vec![],
            step_summary: "6 steps: 4×run_command".to_string(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };
        let no_answer = anyhow::Error::new(crate::agy::transcript::NoAnswerError {
            partial,
            msg: "agy ended the turn without writing a final answer".to_string(),
        });
        let out = resolve_outcome(
            Some(StopReason::Idle),
            b"",
            b"",
            None,
            Some(PathBuf::from("conv-idle")),
            Some("conv-idle".to_string()),
            Some(Err(no_answer)),
            180,
            3600,
        )
        .expect("stopped run with steps recovered as cut_off Ok");
        assert!(out.cut_off);
        assert!(out.text.starts_with(&StopReason::Idle.partial_note()));
        assert!(out.text.contains("idle timeout"));
        assert!(out.text.contains("before the failure: 6 steps"));

        let ok_empty_ans = Transcript {
            answer: String::new(),
            served_model: None,
            artifacts: vec![],
            step_summary: "3 steps".to_string(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };
        let out2 = resolve_outcome(
            Some(StopReason::Idle),
            b"",
            b"",
            None,
            Some(PathBuf::from("conv-idle")),
            Some("conv-idle".to_string()),
            Some(Ok(ok_empty_ans)),
            180,
            3600,
        )
        .expect("stopped run with empty answer Ok transcript recovered as cut_off Ok");
        assert!(out2.cut_off);
        assert!(out2.text.starts_with(&StopReason::Idle.partial_note()));
        assert!(out2.text.contains("idle timeout"));
        assert!(out2.text.contains("before the failure: 3 steps"));
    }

    #[test]
    fn agys_own_print_timeout_marks_the_outcome_cut_off() {
        let stderr: &[u8] =
            b"[agy] print timeout after 10s with turn in progress; returning partial output";
        let out = resolve_outcome(
            None,
            b"a partial answer from stdout",
            stderr,
            None,
            Some(PathBuf::from("conv-trunc")),
            Some("conv-trunc".to_string()),
            None,
            180,
            3600,
        )
        .unwrap();
        assert!(out.cut_off);
        assert!(
            out.text.starts_with("⚠ agy hit its own --print-timeout"),
            "{}",
            out.text
        );
    }

    #[test]
    fn a_clean_finished_run_is_never_cut_off() {
        let out = resolve_outcome(
            None,
            b"a normal clean answer",
            b"",
            None,
            Some(PathBuf::from("conv-ok")),
            Some("conv-ok".to_string()),
            None,
            180,
            3600,
        )
        .unwrap();
        assert!(!out.cut_off);
    }

    #[test]
    fn test_is_wait_only() {
        // Wait messages (should return true)
        assert!(is_wait_only("I will wait for the test suite to complete execution. Once finished, I will receive the results."));
        assert!(is_wait_only("I'll wait for the build to complete."));
        assert!(is_wait_only(
            "I am waiting for the background command to finish."
        ));
        assert!(is_wait_only("バックグラウンドタスクの完了を待機します。"));
        assert!(is_wait_only("ビルドの終了を待ちます。"));
        assert!(is_wait_only("テストの実行結果を待ちます。"));
        assert!(is_wait_only("待機します。"));

        // Regular summaries (should return false)
        assert!(!is_wait_only(""));
        assert!(!is_wait_only(
            "I have implemented the feature. The tests passed successfully."
        ));
        assert!(!is_wait_only("Modified src/main.rs to add the helper function, and cargo build successfully completed."));
        assert!(!is_wait_only(
            "I fixed the bug in helper.rs. All 5 tests passed."
        ));
        assert!(!is_wait_only(
            "ビルドエラーを修正しました。テストはすべて合格しました。"
        ));
        assert!(!is_wait_only(
            "変更内容: src/main.rs の修正。結果: ビルド成功。"
        ));
        assert!(!is_wait_only(
            "I've updated the config, but the build failed with exit code 1."
        ));
    }

    #[test]
    fn waiting_answer_auto_resume_decision_uses_bare_answer() {
        // Real case from conversation 68f84986 (2026-09-14T19:48):
        // agy ended its turn with a wait-only message while a background task was running.
        let bare = "Blenderシミュレーションを再実行しています。完了を待機しています。";
        assert!(is_wait_only(bare), "bare sentence must be wait-only");

        let task = TaskResult {
            id: "68f84986-fb8a-4f02-b918-89d3317a2d96/task-142".to_string(),
            ok: true,
            exit_code: Some(0),
            summary: Some(
                "test result: ok. 5 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out"
                    .to_string(),
            ),
            tail: "Compiling garment v0.1.0 (G:\\workshopG\\gen3dasset\\crates\\garment)\n    \
                   Finished `test` profile [unoptimized + debuginfo] target(s) in 1.16s\n     \
                   Running tests\\pipeline_test.rs\n\ntest test_blender_integration ... ok"
                .to_string(),
            cmd: None,
        };
        // A second task alongside it: `condensed_task_result_lines` clips each
        // excerpt to `TASK_RESULT_TAIL_CHARS`, so a single short task result can
        // now fold to under `is_wait_only`'s 300-char cutoff on its own. Two
        // results keep this test's folded text long regardless of the per-line
        // clip, same as it was before condensation existed.
        let task2 = TaskResult {
            id: "68f84986-fb8a-4f02-b918-89d3317a2d96/task-143".to_string(),
            // A failure keeps its own line (the all-ok pair would fold to one short
            // line), so the folded text stays long.
            ok: false,
            exit_code: Some(101),
            ..task.clone()
        };
        let task3 = TaskResult {
            id: "68f84986-fb8a-4f02-b918-89d3317a2d96/task-144".to_string(),
            ..task2.clone()
        };

        // If evaluated on the folded text (as old code did via `r.text`), the task results
        // block causes `is_wait_only` to fail because it exceeds the 300-char limit.
        let folded = append_task_results(
            bare.to_string(),
            &[task.clone(), task2.clone(), task3.clone()],
        );
        assert!(
            folded.chars().count() > 300,
            "folded text must exceed 300 chars: {}",
            folded.chars().count()
        );
        assert!(
            !is_wait_only(&folded),
            "folded text with task results must NOT pass is_wait_only"
        );

        // Via transcript: resolve_outcome carries bare_answer intact
        let transcript = Transcript {
            answer: bare.to_string(),
            served_model: None,
            artifacts: vec![],
            step_summary: "1 step: 1×run_command".to_string(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![task.clone(), task2.clone(), task3.clone()],
            trailing_quota_retries: 0,
        };
        let out = resolve_outcome(
            None,
            b"",
            b"",
            None,
            None,
            Some("conv-waiting".to_string()),
            Some(Ok(transcript)),
            180,
            3600,
        )
        .unwrap();

        assert_eq!(out.bare_answer, bare);
        assert!(is_wait_only(&out.bare_answer));
        assert!(!is_wait_only(&out.text));
        assert_eq!(out.task_results.len(), 3);

        // Via stdout: resolve_outcome also carries bare_answer intact
        let out_stdout = resolve_outcome(
            None,
            bare.as_bytes(),
            b"",
            None,
            None,
            Some("conv-waiting".to_string()),
            Some(Ok(Transcript {
                answer: String::new(),
                served_model: None,
                artifacts: vec![],
                step_summary: String::new(),
                errors: vec![],
                pending_tasks: vec![],
                task_results: vec![task, task2, task3],
                trailing_quota_retries: 0,
            })),
            180,
            3600,
        )
        .unwrap();

        assert_eq!(out_stdout.bare_answer, bare);
        assert!(is_wait_only(&out_stdout.bare_answer));
        assert!(!is_wait_only(&out_stdout.text));
    }

    fn mk_task(id: &str, exit_code: Option<i32>, ok: bool, tail: &str) -> TaskResult {
        TaskResult {
            id: id.to_string(),
            ok,
            exit_code,
            summary: None,
            tail: tail.to_string(),
            cmd: None,
        }
    }

    #[test]
    fn format_task_result_line_clips_the_tail_and_prefers_exit_code_over_ok_flag() {
        let long_tail = "x".repeat(300);
        let t = mk_task("conv/task-1", Some(0), false, &long_tail);
        let line = format_task_result_line(&t);
        // exit=0 wins over ok:false (same priority as bg_finish_ok).
        assert!(line.starts_with("✓ task-1: ok (exit=0) · "), "{line}");
        // The tail is clipped, not dumped in full.
        assert!(line.chars().count() < 300, "{}", line.chars().count());
        assert!(line.ends_with('…'), "{line}");

        let failed = mk_task("conv/task-2", Some(1), true, "short tail");
        assert!(format_task_result_line(&failed).starts_with("✗ task-2: FAILED"));

        // No exit code known: falls back to the `ok` flag.
        let no_exit_ok = mk_task("conv/task-3", None, true, "ok");
        assert!(format_task_result_line(&no_exit_ok).starts_with("✓ task-3: ok ·"));
        let no_exit_failed = mk_task("conv/task-4", None, false, "bad");
        assert!(format_task_result_line(&no_exit_failed).starts_with("✗ task-4: FAILED ·"));
    }

    #[test]
    fn stop_snapshot_names_step_background_and_shells() {
        let procs: Vec<String> = ["agy.exe", "node.exe", "PowerShell.exe", "cargo.exe"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(shell_names(&procs), vec!["powershell".to_string()]);
        let n = stop_snapshot(
            StopReason::Idle,
            180,
            Some("run_command cargo test"),
            Some(1),
            &procs,
        );
        assert!(n.contains("idle, silent 180s"), "{n}");
        assert!(n.contains("last step: run_command cargo test"), "{n}");
        assert!(n.contains("1 unfinished background task(s)"), "{n}");
        assert!(n.contains("shell alive: powershell"), "{n}");
        let bare = stop_snapshot(StopReason::HardCap, 5, None, Some(0), &[]);
        assert!(
            bare.contains("last step: none") && bare.contains("no shell descendant"),
            "{bare}"
        );
        assert!(bare.contains("no unfinished background task"), "{bare}");
    }

    fn with_cmd(mut t: TaskResult, cmd: &str) -> TaskResult {
        t.cmd = Some(cmd.to_string());
        t
    }

    #[test]
    fn dedupe_keeps_only_the_last_result_of_each_command() {
        let tasks = vec![
            with_cmd(mk_task("c/task-1", Some(1), false, "boom"), "cargo  test"),
            with_cmd(mk_task("c/task-2", Some(0), true, "ok"), "cargo build"),
            with_cmd(mk_task("c/task-3", Some(0), true, "ok"), "cargo test"),
            mk_task("c/task-4", Some(1), false, "no known command"),
        ];
        let ids: Vec<String> = dedupe_task_results(&tasks)
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec!["c/task-2", "c/task-3", "c/task-4"]);
    }

    #[test]
    fn condensed_task_result_lines_one_task_keeps_its_full_line() {
        let tasks = vec![mk_task("conv/task-1", Some(0), true, "ok")];
        let lines = condensed_task_result_lines(&tasks);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("✓ task-1: ok"), "{lines:?}");
    }

    #[test]
    fn condensed_task_result_lines_all_ok_is_one_line_and_a_fixed_failure_is_gone() {
        let tasks = vec![
            with_cmd(mk_task("c/task-1", Some(1), false, "boom"), "cargo test"),
            with_cmd(mk_task("c/task-2", Some(0), true, "ok"), "cargo test"),
            with_cmd(mk_task("c/task-3", Some(0), true, "ok"), "cargo build"),
        ];
        assert_eq!(
            condensed_task_result_lines(&tasks),
            vec!["bg: 2 tasks (3 runs), final all ok".to_string()]
        );
    }

    #[test]
    fn condensed_task_result_lines_lists_only_what_still_fails() {
        let mut tasks: Vec<TaskResult> = (1..=10)
            .map(|i| mk_task(&format!("conv/task-{i}"), Some(0), true, "ok"))
            .collect();
        tasks[4].exit_code = Some(1);
        tasks[5].exit_code = Some(1);
        let lines = condensed_task_result_lines(&tasks);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].contains("task-5:") && lines[0].starts_with('✗'),
            "{lines:?}"
        );
        assert!(lines[1].contains("task-6:"), "{lines:?}");
        assert_eq!(lines[2], "✓ 8 other task(s) ok");
    }

    #[test]
    fn normalize_command_collapses_incidental_whitespace() {
        assert_eq!(
            normalize_command("cargo   test   -p garment"),
            "cargo test -p garment"
        );
        assert_eq!(normalize_command("  cargo test  "), "cargo test");
        assert_eq!(normalize_command(""), "");
    }

    #[test]
    fn repeated_command_warning_fires_at_the_threshold_on_the_most_repeated_line() {
        // 7 repeats: below REPEATED_COMMAND_THRESHOLD (8), stays silent.
        let seven = vec!["gen3dasset.exe sim --decay 0.1".to_string(); 7];
        assert!(repeated_command_warning(&seven).is_none());

        // 8 repeats: fires, names the (clipped) command and the count.
        let eight = vec!["gen3dasset.exe sim --decay 0.1".to_string(); 8];
        let warn = repeated_command_warning(&eight).expect("must fire at the threshold");
        assert!(
            warn.starts_with("⚠ ran \"gen3dasset.exe sim --decay 0.1\" 8 times"),
            "{warn}"
        );

        // Mixed commands: only the most-repeated one crosses the threshold.
        let mut mixed = vec!["cargo check".to_string(); 3];
        mixed.extend(vec!["cargo test -p garment".to_string(); 9]);
        let warn = repeated_command_warning(&mixed).unwrap();
        assert!(
            warn.contains("cargo test -p garment") && warn.contains("9 times"),
            "{warn}"
        );

        // Incidental whitespace differences still count as the same command.
        let mut spaced = vec!["cargo  test".to_string(); 4];
        spaced.extend(vec!["cargo test".to_string(); 4]);
        let warn = repeated_command_warning(&spaced).unwrap();
        assert!(warn.contains("\"cargo test\" 8 times"), "{warn}");

        assert!(repeated_command_warning(&[]).is_none());
    }

    #[test]
    fn test_resolve_outcome_clean_stdout() {
        let stdout = b"Hello, this is the clean answer from stdout!";
        let stderr = b"";

        let transcript = Transcript {
            answer: "fallback answer".to_string(),
            served_model: Some("gemini-1.5-pro".to_string()),
            artifacts: vec![],
            step_summary: "3 steps".to_string(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };

        let outcome = resolve_outcome(
            None,
            stdout,
            stderr,
            None,
            Some(PathBuf::from("conv-123")),
            Some("conv-123".to_string()),
            Some(Ok(transcript)),
            1800,
            3600,
        )
        .unwrap();

        assert_eq!(outcome.text, "Hello, this is the clean answer from stdout!");
        assert_eq!(outcome.via, "stdout");
        assert_eq!(outcome.served_model.as_deref(), Some("gemini-1.5-pro"));
    }

    #[test]
    fn test_resolve_outcome_system_blocks_fallback() {
        let stdout = b"<task-notification>\nbuilding...\n</task-notification>\n   \n";
        let stderr = b"";

        let transcript = Transcript {
            answer: "real fallback answer".to_string(),
            served_model: Some("gemini-1.5-pro".to_string()),
            artifacts: vec![],
            step_summary: "3 steps".to_string(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };

        let outcome = resolve_outcome(
            None,
            stdout,
            stderr,
            None,
            Some(PathBuf::from("conv-123")),
            Some("conv-123".to_string()),
            Some(Ok(transcript)),
            1800,
            3600,
        )
        .unwrap();

        assert_eq!(outcome.text, "real fallback answer");
        assert_eq!(outcome.via, "transcript");
    }

    #[test]
    fn test_resolve_outcome_exit_nonzero_fallback() {
        let stdout = b"Some partial stdout answer";
        let stderr = b"Some error occurred";

        let transcript = Transcript {
            answer: "valid transcript answer despite exit failure".to_string(),
            served_model: Some("gemini-1.5-pro".to_string()),
            artifacts: vec![],
            step_summary: "3 steps".to_string(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };

        let outcome = resolve_outcome(
            None,
            stdout,
            stderr,
            Some(anyhow::anyhow!("exit error")),
            Some(PathBuf::from("conv-123")),
            Some("conv-123".to_string()),
            Some(Ok(transcript)),
            1800,
            3600,
        )
        .unwrap();

        assert_eq!(outcome.text, "valid transcript answer despite exit failure");
        assert_eq!(outcome.via, "transcript");
    }

    #[test]
    fn progress_note_reports_only_what_the_transcript_actually_shows() {
        assert_eq!(progress_note(None), None);

        let empty_t = Transcript {
            answer: String::new(),
            served_model: None,
            artifacts: vec![],
            step_summary: String::new(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };
        assert_eq!(progress_note(Some(&empty_t)), None);

        let steps_t = Transcript {
            answer: String::new(),
            served_model: None,
            artifacts: vec![],
            step_summary: "6 steps: 4×run_command".to_string(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };
        assert_eq!(
            progress_note(Some(&steps_t)).as_deref(),
            Some("before the failure: 6 steps: 4×run_command")
        );

        let arts_t = Transcript {
            answer: String::new(),
            served_model: None,
            artifacts: vec![
                "src/a.rs".into(),
                "src/b.rs".into(),
                "src/c.rs".into(),
                "src/d.rs".into(),
                "src/e.rs".into(),
                "src/f.rs".into(),
                "src/g.rs".into(),
            ],
            step_summary: "6 steps: 4×run_command".to_string(),
            errors: vec!["HTTP 429 quota overloaded".into()],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };
        assert_eq!(
            progress_note(Some(&arts_t)).as_deref(),
            Some("before the failure: 6 steps: 4×run_command · wrote: a.rs, b.rs, c.rs, d.rs, e.rs (+2 more) · error: HTTP 429 quota overloaded")
        );
    }

    #[test]
    fn a_cut_update_stream_is_named_and_the_work_before_it_survives() {
        let stderr: &[u8] = b"error: the connection to the agent was interrupted before the \
            response finished: subscriber fell behind updates, stalled for 6s\n";
        assert!(agy_stream_interrupt_note(&String::from_utf8_lossy(stderr)).is_some());
        assert!(agy_stream_interrupt_note("[agy] print timeout after 10s").is_none());

        // The 2026-09-11 shape: steps on disk, no final answer, exit 1.
        let partial = Transcript {
            answer: String::new(),
            served_model: None,
            artifacts: vec![],
            step_summary: "48 steps: 48×run_command".to_string(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };
        let no_answer = anyhow::Error::new(crate::agy::transcript::NoAnswerError {
            partial,
            msg: "agy ended the turn without writing a final answer".to_string(),
        });
        let err = resolve_outcome(
            None,
            b"",
            stderr,
            Some(anyhow::anyhow!("agy exited with Some(1)")),
            Some(PathBuf::from("conv-cut")),
            Some("conv-cut".to_string()),
            Some(Err(no_answer)),
            180,
            3600,
        )
        .err()
        .expect("a run with no answer failed");
        let text = format!("{err:#}");
        assert!(text.contains("before the failure: 48 steps"), "{text}");
        assert!(text.contains("subscriber fell behind updates"), "{text}");
        assert!(text.contains("agy exited with Some(1)"), "{text}");
    }

    #[test]
    fn a_partial_answer_after_a_stream_cut_is_not_reported_clean() {
        let t = Transcript {
            answer: "half a report".to_string(),
            served_model: None,
            artifacts: vec![],
            step_summary: "3 steps: 3×run_command".to_string(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        };
        let out = resolve_outcome(
            None,
            b"",
            b"error: subscriber fell behind updates, stalled for 5s",
            Some(anyhow::anyhow!("agy exited with Some(1)")),
            Some(PathBuf::from("conv-cut")),
            Some("conv-cut".to_string()),
            Some(Ok(t)),
            180,
            3600,
        )
        .unwrap();
        assert!(
            out.text
                .starts_with("⚠ agy cut its own conversation stream"),
            "{}",
            out.text
        );
        assert!(out.text.ends_with("half a report"), "{}", out.text);
    }

    fn empty_transcript(answer: &str) -> Transcript {
        Transcript {
            answer: answer.to_string(),
            served_model: None,
            artifacts: vec![],
            step_summary: String::new(),
            errors: vec![],
            pending_tasks: vec![],
            task_results: vec![],
            trailing_quota_retries: 0,
        }
    }

    #[test]
    fn agy_error_line_becomes_one_line_with_the_message_first() {
        // The only confirmed shape.
        assert_eq!(
            parse_agy_error("noise\nAGY_ERROR: {\"short_error\":\"model overloaded\"}\n")
                .as_deref(),
            Some("short_error=model overloaded")
        );
        // Unknown keys are carried as-is, message-ish ones lead, non-scalars are skipped.
        let line = parse_agy_error(
            "AGY_ERROR: {\"http_status\":429,\"short_error\":\"rate limited\",\"retryable\":true,\"detail\":{\"x\":1}}",
        )
        .unwrap();
        assert!(line.starts_with("short_error=rate limited"), "{line}");
        assert!(
            line.contains("http_status=429") && line.contains("retryable=true"),
            "{line}"
        );
        assert!(!line.contains("detail"), "{line}");
        // Not JSON: the line as written. No line, or an empty one: nothing.
        assert_eq!(
            parse_agy_error("AGY_ERROR: plain words").as_deref(),
            Some("plain words")
        );
        assert_eq!(parse_agy_error("error: something else"), None);
        assert_eq!(parse_agy_error("AGY_ERROR:   \n"), None);
        // Several (retries): the last one.
        assert_eq!(
            parse_agy_error(
                "AGY_ERROR: {\"short_error\":\"first\"}\nAGY_ERROR: {\"short_error\":\"last\"}"
            )
            .as_deref(),
            Some("short_error=last")
        );
    }

    #[test]
    fn exit_3_keeps_what_agy_said_before_the_error() {
        // 1.2.10: partway through a reply, then a model/agent error. No transcript answer:
        // the stdout fragment must survive in the failure text.
        let err = resolve_outcome(
            None,
            b"the first half of the review",
            b"AGY_ERROR: {\"short_error\":\"model error\"}\n",
            Some(anyhow::anyhow!("agy exited with Some(3)")),
            Some(PathBuf::from("conv-3")),
            Some("conv-3".to_string()),
            Some(Ok(empty_transcript(""))),
            180,
            3600,
        )
        .err()
        .expect("an exit-3 run with no transcript answer is a failure");
        let text = format!("{err:#}");
        assert!(
            text.contains("partial answer: the first half of the review"),
            "{text}"
        );
        assert!(text.contains("agy exited with Some(3)"), "{text}");
    }

    #[test]
    fn an_answer_followed_by_an_agy_error_says_so() {
        let out = resolve_outcome(
            None,
            b"the first half of the review",
            b"AGY_ERROR: {\"short_error\":\"model error\"}\n",
            Some(anyhow::anyhow!("agy exited with Some(3)")),
            Some(PathBuf::from("conv-3")),
            Some("conv-3".to_string()),
            Some(Ok(empty_transcript("the first half of the review"))),
            180,
            3600,
        )
        .unwrap();
        assert!(
            out.text.starts_with(
                "⚠ agy reported an error after this response (AGY_ERROR short_error=model error)"
            ),
            "{}",
            out.text
        );
        assert!(
            out.text.ends_with("the first half of the review"),
            "{}",
            out.text
        );
        assert!(
            !out.cut_off,
            "an error is not a cut the resume nudge can fix"
        );
    }

    #[test]
    /// The soft-deny recovery loop downcasts to `DeniedError`; wrapping the error
    /// in a progress note must not blind it. If this ever fails, the auto-grant
    /// path silently stops recovering.
    fn a_progress_note_does_not_hide_the_denied_error_from_downcast() {
        let denied = DeniedError {
            kind: "run_command".to_string(),
            tool: "run_command".to_string(),
            target: "cargo test".to_string(),
            conv_id: Some("conv-test".to_string()),
        };
        let err: anyhow::Error = anyhow::Error::new(denied);
        let note = "before the failure: 6 steps: 4×run_command · wrote: src/a.rs, src/b.rs";
        let wrapped_err = err.context(note);

        assert!(wrapped_err.downcast_ref::<DeniedError>().is_some());
        let formatted = format!("{wrapped_err:#}");
        assert!(formatted.contains(note));
    }

    // The three states a waiting caller can be in, and the reason the first is not
    // "quiet 0s": with no conversation dir yet there is no liveness source at all,
    // which `should_stop` also refuses to read as idleness. Reporting silence we
    // cannot actually observe would invite exactly the wrong conclusion.
    #[test]
    fn progress_line_distinguishes_no_signal_from_silence() {
        assert_eq!(progress_line(7, None), "agy starting · 7s");
        assert_eq!(progress_line(42, Some(3)), "agy running · 42s");
        assert_eq!(
            progress_line(300, Some(120)),
            "agy running · 300s · quiet 120s"
        );
    }

    #[test]
    fn summarize_verify_output_success_simple() {
        let out = summarize_verify_output("cargo check", 0, "", "   Finished dev profile\n");
        assert_eq!(out, "✓ cargo check: ok");
    }

    #[test]
    fn summarize_verify_output_success_with_test_summary() {
        let stdout = "running 10 tests\n...\ntest result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s\n";
        let out = summarize_verify_output("cargo test", 0, stdout, "");
        assert!(out.starts_with("✓ cargo test: ok (test result: ok. 10 passed; 0 failed"));
        assert!(!out.contains('\n'));
    }

    #[test]
    fn summarize_verify_output_failure_rustc_error() {
        let stderr = concat!(
            "   Compiling my_crate v0.1.0\n",
            "error[E0425]: cannot find value `foo` in this scope\n",
            "  --> src/main.rs:10:5\n",
            "   |\n",
            "10 |     foo();\n",
            "   |     ^^^ not found in this scope\n",
            "\n",
            "error: could not compile `my_crate` (bin \"my_crate\") due to 1 previous error\n"
        );
        let out = summarize_verify_output("cargo check", 101, "", stderr);
        assert_eq!(
            out,
            "✗ cargo check: FAILED (exit 101) · error[E0425]: cannot find value `foo` in this scope"
        );
        assert!(!out.contains('\n'));
    }

    #[test]
    fn summarize_verify_output_failure_test_panic() {
        let stdout = concat!(
            "running 2 tests\n",
            "test tests::test_ok ... ok\n",
            "test tests::test_fail ... FAILED\n\n",
            "failures:\n\n",
            "---- tests::test_fail stdout ----\n",
            "thread 'tests::test_fail' panicked at src/lib.rs:12:9:\n",
            "assertion `left == right` failed\n",
            "  left: 1\n",
            " right: 2\n\n",
            "failures:\n",
            "    tests::test_fail\n\n",
            "test result: FAILED. 1 passed; 1 failed; 0 ignored\n"
        );
        let out = summarize_verify_output("cargo test", 101, stdout, "");
        assert!(out.starts_with("✗ cargo test: FAILED (exit 101) · "));
        assert!(
            out.contains("assertion `left == right` failed")
                || out.contains("panicked at")
                || out.contains("test tests::test_fail ... FAILED")
        );
        assert!(!out.contains('\n'));
    }

    #[test]
    fn summarize_verify_output_failure_mangled_encoding() {
        let mangled = "cargo : \u{FFFD}p\u{FFFD} 'cargo' \u{FFFD}\u{FFFD}A\u{FFFD}R\u{FFFD}}\u{FFFD}h\u{FFFD}\u{FFFD}\u{FFFD}b\u{FFFD}g";
        let out = summarize_verify_output("cargo test", 1, "", mangled);
        assert_eq!(
            out,
            "✗ cargo test: FAILED (exit 1) · ⟨1 line(s) dropped: non-UTF-8 console output⟩"
        );
        assert!(!out.contains('\n'));
    }

    #[test]
    fn summarize_verify_output_failure_typescript() {
        let stdout =
            "src/index.ts:15:3 - error TS2322: Type 'string' is not assignable to type 'number'.\n";
        let out = summarize_verify_output("tsc", 2, stdout, "");
        assert_eq!(
            out,
            "✗ tsc: FAILED (exit 2) · src/index.ts:15:3 - error TS2322: Type 'string' is not assignable to type 'number'."
        );
        assert!(!out.contains('\n'));
    }

    #[test]
    fn summarize_verify_output_token_frugal_bounded_length() {
        let long_err = "error: ".to_string() + &"a".repeat(300) + "\nsecond line\nthird line";
        let out = summarize_verify_output("cargo test --workspace --all-targets", 1, "", &long_err);
        assert!(!out.contains('\n'));
        assert!(!out.contains('\r'));
        assert!(out.len() <= 160);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn image_looks_complete_checks_signature_and_end() {
        // PNG
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(b"IEND\xae\x42\x60\x82");
        assert!(image_looks_complete(&png));
        assert!(!image_looks_complete(&png[..8]));
        assert!(!image_looks_complete(&png[..png.len() - 1]));

        // JPEG
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xD9];
        assert!(image_looks_complete(&jpeg));
        let jpeg_padded = vec![0xFF, 0xD8, 0xFF, 0xD9, 0x00, b' ', b'\n'];
        assert!(image_looks_complete(&jpeg_padded));
        assert!(!image_looks_complete(&[0xFF, 0xD8]));
        assert!(!image_looks_complete(&[0xFF, 0xD8, 0xFF]));
        assert!(!image_looks_complete(&[0xFF, 0xD8, 0x00, 0x00]));

        // GIF
        let gif87 = b"GIF87a;".to_vec();
        assert!(image_looks_complete(&gif87));
        let gif89 = b"GIF89a;".to_vec();
        assert!(image_looks_complete(&gif89));
        assert!(!image_looks_complete(b"GIF87a"));
        assert!(!image_looks_complete(b"GIF89a"));
        assert!(!image_looks_complete(b"GIF89a\x00"));

        // WEBP
        let mut webp = Vec::new();
        webp.extend_from_slice(b"RIFF");
        webp.extend_from_slice(&4u32.to_le_bytes());
        webp.extend_from_slice(b"WEBP");
        assert!(image_looks_complete(&webp));
        assert!(!image_looks_complete(&webp[..11]));
        let mut webp_short = webp.clone();
        webp_short[4..8].copy_from_slice(&100u32.to_le_bytes());
        assert!(!image_looks_complete(&webp_short));

        // Unknown signature and empty bytes
        assert!(!image_looks_complete(&[]));
        assert!(!image_looks_complete(b"UNKNOWN_SIG"));
        assert!(!image_looks_complete(&[0x00, 0x01, 0x02, 0x03]));
    }

    #[test]
    fn image_paths_in_prompt_finds_abs_and_relative() {
        let ws = Path::new("ws_root");
        let abs_str = if cfg!(windows) {
            r"C:\images\photo.png"
        } else {
            "/images/photo.png"
        };
        let prompt = format!(
            r#"Check "{abs_str}", `rel/diagram.jpg`, and another/picture.PNG. Also extra/comma.gif, and ignore notes.txt. Again "{abs_str}""#
        );
        let paths = image_paths_in_prompt(&prompt, ws);
        assert_eq!(
            paths,
            vec![
                PathBuf::from(abs_str),
                ws.join("rel/diagram.jpg"),
                ws.join("another/picture.PNG"),
                ws.join("extra/comma.gif"),
            ]
        );
    }

    #[test]
    fn broken_image_note_names_only_broken_readable_images() {
        let ws = Path::new("test_ws");
        let prompt = "inspect valid.png, broken.jpg, unreadable.gif, and corrupt.webp";

        // Result is None when all images are fine
        let all_fine_read = |_p: &Path| -> Option<Vec<u8>> { Some(vec![0xFF, 0xD8, 0xFF, 0xD9]) };
        assert_eq!(broken_image_note(prompt, ws, all_fine_read), None);

        // Fake read: unreadable is skipped, only broken readable images are named
        let fake_read = |p: &Path| -> Option<Vec<u8>> {
            let s = p.to_string_lossy();
            if s.contains("valid") {
                Some(vec![0xFF, 0xD8, 0xFF, 0xD9])
            } else if s.contains("broken") {
                Some(vec![0xFF, 0xD8]) // broken JPEG
            } else if s.contains("unreadable") {
                None // unreadable
            } else if s.contains("corrupt") {
                Some(b"RIFF\x64\x00\x00\x00WEBP".to_vec()) // truncated WEBP
            } else {
                None
            }
        };
        let note = broken_image_note(prompt, ws, fake_read);
        assert_eq!(
            note,
            Some(format!(
                "image that may not decode: {}, {}",
                ws.join("broken.jpg").display(),
                ws.join("corrupt.webp").display()
            ))
        );
    }
}
