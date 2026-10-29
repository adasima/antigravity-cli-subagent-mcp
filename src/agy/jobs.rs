use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::agy::paths::agy_mcp_home;
use crate::agy::quota::now_unix;
use crate::agy::run::unix_of;

// --- Disk-backed background jobs ----------------------------------------------
// The in-memory job registry (main.rs `State.jobs`) is per-process, so a separate
// `agy-mcp wait` CLI invocation (a different process) can't see it. We mirror each
// async/sync run to disk under ~/.agy-mcp/jobs/<id>.json so the CLI can watch a run
// to completion WITHOUT the supervisor paying LLM tokens to poll. Best-effort: a
// failed write never breaks a run, and the in-memory copy stays authoritative for
// the MCP agy_poll fast path.

fn jobs_dir() -> PathBuf {
    agy_mcp_home().join("jobs")
}
fn job_record_path(id: &str) -> PathBuf {
    jobs_dir().join(format!("{id}.json"))
}
/// `<id>.waiter` holds the PID of an `agy-mcp wait` watching the job, so `agy_poll`
/// can say polling is redundant (a model that started the wait otherwise keeps
/// polling "to be safe" — 2026-10-20, six wasted calls on one job). A stale file
/// is harmless: its PID is dead, and `prune_job_records` sweeps it.
fn job_waiter_path(id: &str) -> PathBuf {
    jobs_dir().join(format!("{id}.waiter"))
}

/// Mark this process as watching `id` (best-effort).
pub fn mark_job_waited(id: &str) {
    let _ = std::fs::write(job_waiter_path(id), std::process::id().to_string());
}

pub fn clear_job_waited(id: &str) {
    let _ = std::fs::remove_file(job_waiter_path(id));
}

/// Is a live `agy-mcp wait` watching `id`?
pub fn job_is_waited(id: &str, alive: &dyn Fn(u32) -> bool) -> bool {
    std::fs::read_to_string(job_waiter_path(id))
        .is_ok_and(|s| waiter_pid_is_live(&s, alive))
}

fn waiter_pid_is_live(contents: &str, alive: &dyn Fn(u32) -> bool) -> bool {
    contents.trim().parse::<u32>().is_ok_and(alive)
}

pub fn reports_dir() -> PathBuf {
    let p = agy_mcp_home().join("reports");
    let _ = std::fs::create_dir_all(&p);
    p
}

/// One background run, mirrored to disk for cross-process `agy-mcp wait`.
///
/// The record doubles as the **dispatch spec** (発注書): it carries the full
/// prompt, the workspace and the conversation id, so a job whose supervisor died
/// with it (session restart, server crash) can be resumed — or re-dispatched
/// verbatim — without paying tokens to reconstruct what was asked. Everything
/// added after the first version is `#[serde(default)]` so older records load.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct JobRecord {
    pub id: String,
    /// "running" | "done" | "failed".
    pub status: String,
    pub tool: String,
    pub model: String,
    pub started_unix: u64,
    pub finished_unix: Option<u64>,
    pub prompt_head: String,
    /// Final report (done) or error message (failed); None while running.
    pub result: Option<String>,
    /// The prompt exactly as dispatched — the part that costs real tokens to
    /// rebuild. `prompt_head` stays for one-line listings.
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub add_dirs: Vec<String>,
    #[serde(default)]
    pub preset: String,
    /// agy conversation this job runs in, recorded as soon as the conv dir is
    /// detected (mid-run) so a lost job can be RESUMED, not just re-run.
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// PID of the agy-mcp server that owns this job. Several servers (one per
    /// Claude Code session) share the mirror dir, so startup cleanup must demote
    /// only the records whose owner is actually gone.
    #[serde(default)]
    pub owner_pid: Option<u32>,
    /// Unix time of the last observed agy liveness (conv-store / brain write).
    /// A running job whose liveness is old is stalled — the signal that turns
    /// "running 30 min" into "dead 29 min ago".
    #[serde(default)]
    pub last_activity_unix: Option<u64>,
    /// True once this job's terminal result actually reached a caller — via
    /// `agy_poll`, an inline return, or the `wait`/`poll` CLI.
    ///
    /// It lives on the record rather than only in the server's memory because the
    /// recommended collection path (`agy-mcp wait <id>` as a background task) is a
    /// DIFFERENT PROCESS: with the flag in memory only, collecting via the CLI left
    /// the job looking uncollected and the next `agy_sub` warned about a result the
    /// caller already had (observed 2026-07-26). A banner that cries wolf gets
    /// ignored, which costs exactly the visibility it exists to provide.
    #[serde(default)]
    pub seen: bool,
    /// One-line description of the most recent tool call, e.g. "run_command:
    /// cargo test -p garment (95s ago)" — mirrored on the job-mirror tick so a
    /// supervisor running several jobs in parallel can tell what each is doing
    /// RIGHT NOW instead of only "running Ns". None until the first tick lands,
    /// and for any record written before this field existed.
    #[serde(default)]
    pub last_step: Option<String>,
}
impl JobRecord {
    /// Run duration: finished − started once terminal, else elapsed-so-far.
    pub fn secs(&self) -> u64 {
        self.finished_unix
            .unwrap_or_else(now_unix)
            .saturating_sub(self.started_unix)
    }
    pub fn is_running(&self) -> bool {
        self.status == "running"
    }
    /// Seconds since agy last showed liveness, for a *running* job (None if the
    /// job is terminal or liveness was never observed).
    pub fn silent_s(&self) -> Option<u64> {
        if !self.is_running() {
            return None;
        }
        Some(now_unix().saturating_sub(self.last_activity_unix?))
    }
    /// What to do with a job that died before returning its result. Resuming
    /// keeps whatever the sub-agent already did; the spec pointer makes a clean
    /// re-dispatch free.
    pub fn recovery_hint(&self) -> String {
        let ws = if self.workspace.is_empty() {
            "<workspace>"
        } else {
            &self.workspace
        };
        match &self.conversation_id {
            Some(c) => format!(
                "RESUME (keeps the work already done): agy_sub(conversation_id=\"{c}\", \
                 workspace=\"{ws}\", prompt=\"Continue the task from where it stopped.\")\n\
                 Full dispatch spec incl. the original prompt: `agy-mcp show {}`",
                self.id
            ),
            None if self.prompt.is_empty() => format!(
                "Neither a conversation nor the prompt was saved for {} (the record predates \
                 dispatch-spec persistence) — this one has to be re-dispatched by hand.",
                self.id
            ),
            None => format!(
                "No conversation was recorded (it died before agy opened one) — re-dispatch \
                 from the saved spec: `agy-mcp show {}`",
                self.id
            ),
        }
    }

    /// [`Self::recovery_hint`], adjusted for how the run died. Pure.
    ///
    /// A plain resume is the right default, but not after agy cut its own stream:
    /// on 2026-09-11 the three resumes of such a run were cut the same way within
    /// 16–127s, each spending quota to rediscover it. Resume is still worth one try
    /// (the cut is agy's, and a later agy may not do it), but the reader needs the
    /// way out that does not go through the same conversation.
    pub fn recovery_hint_after(&self, error: &str) -> String {
        let hint = self.recovery_hint();
        match (
            &self.conversation_id,
            crate::agy::run::agy_stream_interrupt_note(error),
        ) {
            (Some(c), Some(_)) => format!(
                "{hint}\nIf the resume is cut the same way, START FRESH instead: read what it \
                 found with agy_diag(action=trace, conversation_id=\"{c}\") and seed a new \
                 agy_sub with that."
            ),
            _ => hint,
        }
    }
}

/// Mirror a job to disk (best-effort).
pub fn write_job_record(rec: &JobRecord) {
    let _ = std::fs::create_dir_all(jobs_dir());
    if let Ok(s) = serde_json::to_string_pretty(rec) {
        let _ = std::fs::write(job_record_path(&rec.id), s);
    }
}

/// Atomically claim a free job id in the shared mirror dir.
///
/// Several agy-mcp servers (one per Claude Code session) now coexist and share
/// `~/.agy-mcp/jobs/`, so a per-process counter alone would let two servers both
/// pick `j1` and silently overwrite each other's record (observed). `create_new`
/// makes the claim a filesystem-level compare-and-swap: whoever creates the file
/// owns the id. The placeholder is empty until `write_job_record` fills it — an
/// unparseable record is already treated as absent everywhere.
pub fn claim_job_id() -> String {
    let _ = std::fs::create_dir_all(jobs_dir());
    let mut n = next_job_seq_hint();
    for _ in 0..1000 {
        let id = format!("j{n}");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(job_record_path(&id))
        {
            Ok(_) => return id,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(_) => break, // unusable dir: fall through to a pid-qualified id
        }
    }
    format!("j{}x{n}", std::process::id())
}

/// One past the highest `j<N>` currently on disk (1 when the dir is empty).
fn next_job_seq_hint() -> u64 {
    let highest = std::fs::read_dir(jobs_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.path()
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .filter_map(|stem| stem.strip_prefix('j')?.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    highest + 1
}

/// Read-modify-write one job record (best-effort; no-op if absent/corrupt).
pub fn update_job_record(id: &str, f: impl FnOnce(&mut JobRecord)) {
    if let Some(mut rec) = read_job_record(id) {
        f(&mut rec);
        write_job_record(&rec);
    }
}

/// Record mid-run progress on a job: the conversation it landed in (so a lost
/// job is resumable), the last liveness timestamp (so a stalled job is visible
/// as stalled), and the current-step description (so a running job says what
/// it's doing, not just that it's running). Never touches a record that already
/// went terminal. Each of the three is independently optional — a caller that
/// only has one fact (e.g. the conv-dir-detection tick has no step line yet)
/// passes None for the others rather than overwriting them with nothing.
pub fn note_job_progress(
    job_id: &str,
    conv_id: Option<&str>,
    last_activity: Option<u64>,
    last_step: Option<&str>,
) {
    update_job_record(job_id, |rec| {
        if !rec.is_running() {
            return;
        }
        if let Some(c) = conv_id {
            rec.conversation_id = Some(c.to_string());
        }
        if let Some(a) = last_activity {
            rec.last_activity_unix = Some(a);
        }
        if let Some(s) = last_step {
            rec.last_step = Some(s.to_string());
        }
    });
}

/// Mark a job's result as collected on disk. Idempotent, best-effort, and a no-op
/// for a still-running job (a running record has no result to have been collected).
///
/// Called from every path that actually hands a terminal result to somebody: the
/// server's `mark_seen` and the `wait`/`poll` CLI. The CLI is the reason this is on
/// disk at all — see [`JobRecord::seen`].
pub fn mark_job_seen(id: &str) {
    update_job_record(id, |rec| {
        if !rec.is_running() {
            rec.seen = true;
        }
    });
}

/// True when two workspace paths name the same directory, as far as a string
/// compare can tell: trailing separators ignored, and case-insensitive on Windows.
///
/// Deliberately NOT canonicalizing. This compares a job record written by another
/// process against the current cwd, and a canonicalize would touch the filesystem
/// (failing for a workspace that has since been moved or unmounted) to sharpen a
/// heuristic whose job is only to scope a listing. Pure so it can be unit-tested.
pub fn same_workspace_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches(['/', '\\']).to_string();
    let (a, b) = (norm(a), norm(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if cfg!(windows) {
        a.replace('/', "\\")
            .eq_ignore_ascii_case(&b.replace('/', "\\"))
    } else {
        a == b
    }
}

/// Job ids (other than `this_id`) that ran in the SAME workspace with a run
/// window overlapping `[start, end]`.
///
/// This is what tells a run's own git-diff-only changes (paths git saw but no
/// transcript receipt claimed) apart from a genuinely unattributable one: if
/// nothing else was running here at the same time, those changes are still this
/// run's, just made via a shell command instead of agy's own write tool. If a
/// sibling job (or another agy-mcp process's job, since the disk mirror is the
/// only place those exist) was live in the same tree, the diff can't be pinned on
/// either one.
///
/// A still-running sibling (`finished_unix == None`) is treated as ending at
/// "now or later" (`u64::MAX`) rather than requiring a live clock reading here —
/// it is still running, so it necessarily overlaps anything that started before
/// it finishes, which for an open-ended run is unconditionally true once it has
/// started. Pure and unit-tested; the disk read (`list_job_records`) stays in the
/// caller.
pub fn overlapping_jobs(
    this_id: &str,
    workspace: &str,
    start: u64,
    end: u64,
    records: &[JobRecord],
) -> Vec<String> {
    let mut v: Vec<&JobRecord> = records
        .iter()
        .filter(|r| r.id != this_id && !r.id.is_empty())
        .filter(|r| same_workspace_path(&r.workspace, workspace))
        .filter(|r| {
            let other_end = r.finished_unix.unwrap_or(u64::MAX);
            r.started_unix <= end && other_end >= start
        })
        .collect();
    v.sort_by_key(|r| r.started_unix);
    v.into_iter().map(|r| r.id.clone()).collect()
}

/// Jobs other than `this_id` still running in `workspace`, oldest first — what a
/// new dispatch will share a working tree with. A record whose supervising server
/// died without demoting it (that sweep runs only at startup) is not running —
/// same test as [`should_demote_job`].
pub fn running_in_workspace<'a>(
    this_id: &str,
    workspace: &str,
    records: &'a [JobRecord],
    owner_alive: &dyn Fn(u32) -> bool,
) -> Vec<&'a JobRecord> {
    let mut v: Vec<&JobRecord> = records
        .iter()
        .filter(|r| r.id != this_id && !r.id.is_empty() && r.is_running())
        .filter(|r| !should_demote_job(r, owner_alive))
        .filter(|r| same_workspace_path(&r.workspace, workspace))
        .collect();
    v.sort_by_key(|r| r.started_unix);
    v
}

/// One line naming the jobs a new dispatch shares a working tree with, and what
/// each has written so far. Parallel lanes in one tree contaminated each other's
/// builds and one reverted the other's files as "changes I didn't make"
/// (gen3dasset, 2026-09); the order is where to prevent that, and this line is
/// the reminder when the order forgot. `paths` are already workspace-relative.
pub fn shared_tree_note(siblings: &[(&JobRecord, Vec<String>)]) -> Option<String> {
    const SHOWN: usize = 5;
    if siblings.is_empty() {
        return None;
    }
    let jobs: Vec<String> = siblings
        .iter()
        .map(|(r, paths)| {
            let head: String = r.prompt_head.chars().take(40).collect();
            let wrote = if paths.is_empty() {
                "no writes yet".to_string()
            } else {
                let mut s = paths
                    .iter()
                    .take(SHOWN)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                if paths.len() > SHOWN {
                    s += &format!(" +{}", paths.len() - SHOWN);
                }
                format!("wrote: {s}")
            };
            format!("{} \"{}\" ({wrote})", r.id, head.trim())
        })
        .collect();
    Some(format!(
        "⚠ same working tree as running job(s): {} — tell each order what the other may \
         not touch, or give it its own worktree.",
        jobs.join("; ")
    ))
}

/// How long after finishing an uncollected result is still worth handing back.
/// Matches the server's uncollected-job banner window: past this the caller has
/// almost certainly moved on, and re-printing a stale report is noise.
pub const WAIT_UNCOLLECTED_WINDOW_S: u64 = 1800;

/// Pick the jobs an argument-less `agy-mcp wait` should account for, oldest-first.
///
/// Two kinds qualify, and both are things the caller is still owed:
///   * **running** — the ordinary "wait for the job I just started" case;
///   * **terminal but `seen == false`, finished recently** — a result nobody has
///     collected yet, which `wait` can hand back immediately.
///
/// Scoped to one workspace when `ws` is given, because the mirror dir is shared by
/// every session on the machine and waiting on another project's run is never what
/// was meant.
///
/// It returns ALL of them rather than the newest one on purpose. Runs are dispatched
/// up to `AGY_MAX_CONCURRENT` at a time, so "the latest job" would make three
/// background `wait`s launched after a three-way fanout all watch the same run —
/// the caller believing it awaited three results while two ran unwatched, silently.
/// Returning the set keeps no-arg `wait` correct under the concurrency the bridge
/// is built around, and lets the caller see the roster of what it's waiting for.
pub fn select_wait_targets(recs: &[JobRecord], ws: Option<&str>, now: u64) -> Vec<String> {
    let mut v: Vec<&JobRecord> = recs
        .iter()
        .filter(|r| !r.id.is_empty())
        .filter(|r| match ws {
            Some(w) => same_workspace_path(&r.workspace, w),
            None => true,
        })
        .filter(|r| {
            if r.is_running() {
                return true;
            }
            if r.seen {
                return false;
            }
            let age = now.saturating_sub(r.finished_unix.unwrap_or(0));
            age <= WAIT_UNCOLLECTED_WINDOW_S
        })
        .collect();
    v.sort_by_key(|r| r.started_unix);
    v.into_iter().map(|r| r.id.clone()).collect()
}

/// Read a job's disk record (None if absent/corrupt).
pub fn read_job_record(id: &str) -> Option<JobRecord> {
    std::fs::read_to_string(job_record_path(id))
        .ok()
        .and_then(|s| serde_json::from_str::<JobRecord>(&s).ok())
}

/// All disk job records, newest-started first.
pub fn list_job_records() -> Vec<JobRecord> {
    let entries: Vec<std::path::PathBuf> = std::fs::read_dir(jobs_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| e.path())
        .collect();

    if entries.is_empty() {
        return Vec::new();
    }

    let chunk_size = (entries.len()
        / std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4))
    .max(1);

    let mut v: Vec<JobRecord> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        for chunk in entries.chunks(chunk_size) {
            handles.push(s.spawn(move || {
                let mut local_v = Vec::with_capacity(chunk.len());
                for path in chunk {
                    if let Ok(str_data) = std::fs::read_to_string(path) {
                        if let Ok(rec) = serde_json::from_str::<JobRecord>(&str_data) {
                            local_v.push(rec);
                        }
                    }
                }
                local_v
            }));
        }

        let mut results = Vec::with_capacity(entries.len());
        for handle in handles {
            if let Ok(mut local_v) = handle.join() {
                results.append(&mut local_v);
            }
        }
        results
    });

    v.sort_by(|a, b| b.started_unix.cmp(&a.started_unix));
    v
}

pub fn prune_reports() {
    let cutoff = now_unix().saturating_sub(30 * 24 * 3600);
    for e in std::fs::read_dir(reports_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        if e.metadata()
            .and_then(|m| m.modified())
            .map(|m| unix_of(m) < cutoff)
            .unwrap_or(false)
        {
            let p = e.path();
            if std::fs::symlink_metadata(&p).is_ok_and(|m| !m.is_symlink()) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// Drop terminal job records older than 24h so the dir doesn't grow forever.
/// Also clears old unreadable files — `claim_job_id` leaves an empty placeholder
/// that a process dying before its first write would otherwise strand for good.
pub fn prune_job_records() {
    let cutoff = now_unix().saturating_sub(24 * 3600);
    for rec in list_job_records() {
        if !rec.is_running() && rec.finished_unix.unwrap_or(rec.started_unix) < cutoff {
            let p = job_record_path(&rec.id);
            if std::fs::symlink_metadata(&p).is_ok_and(|m| !m.is_symlink()) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    for e in std::fs::read_dir(jobs_dir())
        .into_iter()
        .flatten()
        .flatten()
    {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "waiter")
            && !std::fs::read_to_string(&p)
                .is_ok_and(|s| waiter_pid_is_live(&s, &crate::is_pid_alive))
        {
            let _ = std::fs::remove_file(&p);
            continue;
        }
        if p.extension().is_some_and(|x| x == "json")
            && e.metadata()
                .and_then(|m| m.modified())
                .map(|m| unix_of(m) < cutoff)
                .unwrap_or(false)
            && std::fs::read_to_string(&p)
                .ok()
                .and_then(|s| serde_json::from_str::<JobRecord>(&s).ok())
                .is_none()
            && std::fs::symlink_metadata(&p).is_ok_and(|m| !m.is_symlink())
        {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Should a startup sweep demote this record to `failed`?
///
/// Only when its owner is really gone. Servers are per-session and now coexist,
/// so a blanket "every on-disk running is orphaned" would let a starting session
/// kill a *live* sibling's jobs. Legacy records (no `owner_pid`) keep the old
/// behavior — they predate multi-instance and have no owner to check.
pub fn should_demote_job(rec: &JobRecord, is_alive: &dyn Fn(u32) -> bool) -> bool {
    rec.is_running() && !rec.owner_pid.is_some_and(is_alive)
}

/// Build the failure text for a job whose owning server exited. Pure so both
/// `mark_orphaned_running` and the `cli_wait` inline path share the same
/// format, and so it can be unit-tested without disk or process state.
///
/// Without this the "last seen" line is either duplicated between the two
/// callers (and inevitably diverges) or absent from one of them.
pub fn format_lost_job_text(
    silent_s: Option<u64>,
    last_step: Option<&str>,
    written: &[String],
    recovery_hint: &str,
) -> String {
    let silent = silent_s
        .map(|s| format!(" agy had shown no activity for {s}s before that."))
        .unwrap_or_default();
    let last_seen = last_step
        .map(|s| format!("\nlast seen: {s}"))
        .unwrap_or_default();
    let written = written_before_stop_line(written);
    format!(
        "job lost: the agy-mcp server that owned it exited while it was running.{silent}{last_seen}{written}\n{recovery_hint}"
    )
}

fn make_relative_to_workspace(path_str: &str, workspace: &str) -> String {
    if workspace.is_empty() {
        return path_str.to_string();
    }
    let p = std::path::Path::new(path_str);
    let ws = std::path::Path::new(workspace);
    if let Ok(rel) = p.strip_prefix(ws) {
        return rel.to_string_lossy().into_owned();
    }
    let norm = |s: &str| {
        let s = s.trim().replace('\\', "/");
        if cfg!(windows) {
            s.to_ascii_lowercase()
        } else {
            s
        }
    };
    let ws_n = norm(workspace);
    let p_n = norm(path_str);
    let ws_n_trimmed = ws_n.trim_end_matches('/');
    if p_n.starts_with(ws_n_trimmed)
        && (p_n.len() == ws_n_trimmed.len() || p_n.as_bytes().get(ws_n_trimmed.len()) == Some(&b'/'))
    {
        let slashed = path_str.replace('\\', "/");
        if let Some(rel) = slashed.get(ws_n_trimmed.len()..) {
            let rel = rel.trim_start_matches('/');
            if !rel.is_empty() {
                return rel.to_string();
            }
        }
    }
    path_str.to_string()
}

/// `"\nwritten before the stop (may be half-done): a.rs, b.rs"`, or `""` when
/// nothing was written. Shows at most `LOST_JOB_WRITTEN_CAP` paths, then
/// `" (+N more)"`. Pure.
fn written_before_stop_line(written: &[String]) -> String {
    if written.is_empty() {
        return String::new();
    }
    let shown = written
        .iter()
        .take(LOST_JOB_WRITTEN_CAP)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let more = if written.len() > LOST_JOB_WRITTEN_CAP {
        format!(" (+{} more)", written.len() - LOST_JOB_WRITTEN_CAP)
    } else {
        String::new()
    };
    format!("\nwritten before the stop (may be half-done): {shown}{more}")
}

const LOST_JOB_WRITTEN_CAP: usize = 8;

impl JobRecord {
    /// Paths named by this job's own write receipts — what a lost job leaves
    /// half-done. Reads the conversation's transcript (`conversation_id` →
    /// `resolve_conv_dir` → `read_full`), keeps receipts whose file mtime is at or
    /// after `started_unix` (a resumed conversation's earlier writes drop out),
    /// de-duplicates keeping first-seen order, and makes each path relative to
    /// `workspace` when it lies under it. Empty when the conversation or its
    /// transcript can't be found. Best-effort, reads disk.
    pub fn written_paths(&self) -> Vec<String> {
        let conv_id = match &self.conversation_id {
            Some(id) if !id.is_empty() => id,
            _ => return Vec::new(),
        };
        let conv_dir = match crate::agy::paths::resolve_conv_dir(conv_id) {
            Some(dir) => dir,
            None => return Vec::new(),
        };
        let transcript = match crate::agy::transcript::read_full(&conv_dir) {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for art in transcript.artifacts {
            let path = std::path::Path::new(&art);
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            if unix_of(mtime) < self.started_unix {
                continue;
            }

            let rel = if !self.workspace.is_empty() {
                make_relative_to_workspace(&art, &self.workspace)
            } else {
                art.clone()
            };

            if seen.insert(rel.clone()) {
                out.push(rel);
            }
        }
        out
    }
}

/// On server startup, demote *orphaned* `running` records to `failed` — a job
/// whose owning process is gone will never report, and `agy-mcp wait` would
/// block on it forever. The failure text carries the recovery spec (resume by
/// conversation id, or re-dispatch from the saved prompt) so a job lost with its
/// supervisor doesn't cost tokens to reconstruct. Also prunes stale records.
pub fn mark_orphaned_running(is_alive: &dyn Fn(u32) -> bool) {
    for mut rec in list_job_records() {
        if should_demote_job(&rec, is_alive) {
            let text = format_lost_job_text(
                rec.silent_s(),
                rec.last_step.as_deref(),
                &rec.written_paths(),
                &rec.recovery_hint(),
            );
            rec.status = "failed".into();
            rec.finished_unix = Some(now_unix());
            rec.result = Some(text);
            write_job_record(&rec);
        }
    }
    prune_job_records();
    prune_reports();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_same_workspace_path() {
        assert!(same_workspace_path(r"C:\proj", r"C:\proj\"));
        assert!(same_workspace_path(r"C:\proj", " C:\\proj "));
        assert!(!same_workspace_path(r"C:\proj", r"C:\other"));
        // An empty side can't identify a workspace — a record that never recorded
        // one must not match the cwd of whoever happens to ask.
        assert!(!same_workspace_path("", r"C:\proj"));
        assert!(!same_workspace_path(r"C:\proj", ""));
        // Windows: case and separator flavor are both noise. Elsewhere case matters.
        if cfg!(windows) {
            assert!(same_workspace_path(r"c:\Proj\sub", "C:/PROJ/sub"));
        } else {
            assert!(!same_workspace_path("/proj", "/PROJ"));
        }
    }

    /// Concurrent-attribution evidence for the footer's "other jobs ran meanwhile"
    /// note: same workspace, overlapping time window, excluding the job asking.
    #[test]
    fn test_overlapping_jobs() {
        let mk = |id: &str, ws: &str, started: u64, fin: Option<u64>| JobRecord {
            id: id.into(),
            workspace: ws.into(),
            started_unix: started,
            finished_unix: fin,
            ..Default::default()
        };
        let recs = vec![
            mk("j1", r"C:\proj", 100, Some(200)),  // this job itself
            mk("j2", r"C:\proj", 150, Some(180)),  // inside the window -> overlaps
            mk("j3", r"C:\proj", 50, Some(90)),    // finished before the window opened -> no
            mk("j4", r"C:\proj", 190, Some(500)),  // starts inside, finishes after -> overlaps
            mk("j5", r"C:\proj", 300, None),       // starts after the window closed -> no
            mk("j6", r"C:\other", 150, Some(180)), // same window, different workspace -> no
            mk("j7", r"C:\proj", 150, None),       // still running, started inside -> overlaps
        ];
        assert_eq!(
            overlapping_jobs("j1", r"C:\proj", 100, 200, &recs),
            vec!["j2", "j7", "j4"]
        );
        // Nothing else ever ran here.
        assert!(overlapping_jobs("j1", r"C:\nope", 100, 200, &[]).is_empty());
    }

    #[test]
    fn running_siblings_in_the_same_tree_are_named_with_their_writes() {
        let mk = |id: &str, ws: &str, status: &str, started: u64, owner: Option<u32>| JobRecord {
            id: id.into(),
            workspace: ws.into(),
            status: status.into(),
            started_unix: started,
            owner_pid: owner,
            prompt_head: format!("task {id}"),
            ..Default::default()
        };
        let recs = vec![
            mk("j1", r"C:\proj", "running", 100, Some(7)), // the job asking
            mk("j3", r"C:\proj", "running", 300, Some(7)),
            mk("j2", r"C:\proj\", "running", 200, Some(7)), // trailing separator: same tree
            mk("j4", r"C:\proj", "done", 150, Some(7)),
            mk("j5", r"C:\other", "running", 150, Some(7)),
            mk("j6", r"C:\proj", "running", 150, Some(9)), // owner gone
            mk("j8", r"C:\proj", "running", 150, None),    // legacy, no owner: treated as gone
        ];
        let alive = |pid: u32| pid == 7;
        let ids: Vec<&str> = running_in_workspace("j1", r"C:\proj", &recs, &alive)
            .iter()
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(ids, vec!["j2", "j3"]);

        let sib = running_in_workspace("j1", r"C:\proj", &recs, &alive);
        let many: Vec<String> = (1..=7).map(|i| format!("f{i}.rs")).collect();
        let note = shared_tree_note(&[(sib[0], vec![]), (sib[1], many)]).unwrap();
        assert!(
            note.starts_with("⚠ same working tree as running job(s): "),
            "{note}"
        );
        assert!(note.contains("j2 \"task j2\" (no writes yet)"), "{note}");
        assert!(
            note.contains("j3 \"task j3\" (wrote: f1.rs, f2.rs, f3.rs, f4.rs, f5.rs +2)"),
            "{note}"
        );
        assert_eq!(shared_tree_note(&[]), None);
    }

    /// No-arg `wait` must account for EVERY outstanding job in this workspace, not
    /// just the newest — see [`select_wait_targets`] for why picking one silently
    /// breaks parallel fanout.
    #[test]
    fn test_select_wait_targets() {
        let now = 1_000_000u64;
        let rec = |id: &str, status: &str, ws: &str, started: u64, fin: Option<u64>, seen: bool| {
            JobRecord {
                id: id.into(),
                status: status.into(),
                workspace: ws.into(),
                started_unix: started,
                finished_unix: fin,
                seen,
                ..Default::default()
            }
        };
        let recs = vec![
            // Newest-first, the order list_job_records hands back.
            rec("j5", "running", r"C:\proj", now - 10, None, false),
            rec("j4", "done", r"C:\proj", now - 100, Some(now - 60), false),
            rec("j3", "done", r"C:\proj", now - 200, Some(now - 150), true),
            rec(
                "j2",
                "failed",
                r"C:\other",
                now - 300,
                Some(now - 250),
                false,
            ),
            rec(
                "j1",
                "done",
                r"C:\proj",
                now - 9000,
                Some(now - 8000),
                false,
            ),
        ];
        // Scoped to the workspace, oldest-first: the running job and the uncollected
        // result. j3 was collected, j2 belongs to another project, j1 finished long
        // outside the window.
        assert_eq!(
            select_wait_targets(&recs, Some(r"C:\proj"), now),
            vec!["j4", "j5"]
        );
        // A failed-but-uncollected job counts too — a failure is a result to hand back.
        assert_eq!(
            select_wait_targets(&recs, Some(r"C:\other"), now),
            vec!["j2"]
        );
        // Unscoped sweeps every workspace.
        assert_eq!(
            select_wait_targets(&recs, None, now),
            vec!["j2", "j4", "j5"]
        );
        // Nothing outstanding here.
        assert!(select_wait_targets(&recs, Some(r"C:\nope"), now).is_empty());
        // A placeholder claimed by `claim_job_id` but not yet filled in parses as an
        // empty record; it must not become a target we'd block on forever.
        assert!(select_wait_targets(&[JobRecord::default()], None, now).is_empty());
    }

    /// `mark_job_seen` is the cross-process half of the uncollected-result banner.
    #[test]
    fn test_mark_job_seen_only_applies_to_terminal() {
        let mut running = JobRecord {
            status: "running".into(),
            ..Default::default()
        };
        let mut done = JobRecord {
            status: "done".into(),
            ..Default::default()
        };
        // Same predicate mark_job_seen applies, exercised without touching disk.
        for rec in [&mut running, &mut done] {
            if !rec.is_running() {
                rec.seen = true;
            }
        }
        assert!(
            !running.seen,
            "a running job has no result to have been collected"
        );
        assert!(done.seen);
    }

    /// A lost job must be recoverable from its record alone — resume when we know
    /// the conversation, re-dispatch from the saved prompt otherwise. This is the
    /// difference between "Re-run it." and not having to pay for it twice.
    #[test]
    fn job_record_carries_its_own_recovery() {
        let mut rec = JobRecord {
            id: "j7".into(),
            status: "running".into(),
            workspace: r"C:\proj".into(),
            last_activity_unix: Some(now_unix().saturating_sub(90)),
            ..Default::default()
        };
        // A running job reports how long agy has been silent.
        assert_eq!(rec.silent_s(), Some(90));
        // Nothing saved at all (a legacy record) => say so, don't point at a spec
        // that isn't there.
        let hint = rec.recovery_hint();
        assert!(hint.contains("re-dispatched by hand"), "{hint}");

        // Prompt saved but no conversation yet => re-dispatch from the spec.
        rec.prompt = "do the thing".into();
        let hint = rec.recovery_hint();
        assert!(hint.contains("agy-mcp show j7"), "{hint}");
        assert!(!hint.contains("conversation_id=\""), "{hint}");

        rec.conversation_id = Some("abc-123".into());
        let hint = rec.recovery_hint();
        assert!(hint.contains("conversation_id=\"abc-123\""), "{hint}");
        assert!(hint.contains(r"C:\proj"), "{hint}");

        // Silence is only meaningful while running.
        rec.status = "done".into();
        assert_eq!(rec.silent_s(), None);
    }

    #[test]
    fn a_stream_cut_adds_the_way_out_that_skips_the_conversation() {
        let rec = JobRecord {
            id: "j1".into(),
            workspace: r"C:\proj".into(),
            prompt: "p".into(),
            conversation_id: Some("abc-123".into()),
            ..Default::default()
        };
        assert_eq!(
            rec.recovery_hint_after("agy exited with Some(2)"),
            rec.recovery_hint()
        );

        let cut = rec.recovery_hint_after(
            "stderr: error: the connection to the agent was interrupted before the response \
             finished: subscriber fell behind updates, stalled for 6s",
        );
        assert!(
            cut.starts_with(&rec.recovery_hint()),
            "resume stays the first try: {cut}"
        );
        assert!(cut.contains("START FRESH"), "{cut}");
        assert!(cut.contains("conversation_id=\"abc-123\""), "{cut}");

        // Nothing to trace without a conversation: the plain hint is all there is.
        let no_conv = JobRecord {
            conversation_id: None,
            ..rec.clone()
        };
        assert_eq!(
            no_conv.recovery_hint_after("subscriber fell behind updates"),
            no_conv.recovery_hint()
        );
    }

    /// The lost-job text must include `last seen: …` when last_step is present,
    /// and omit it cleanly when absent — both mark_orphaned_running and the
    /// cli_wait inline path rely on this single formatter.
    #[test]
    fn format_lost_job_text_includes_last_step_when_present() {
        let hint = "RESUME …";

        // No silence, no step.
        let t = format_lost_job_text(None, None, &[], hint);
        assert!(t.starts_with("job lost:"), "{t}");
        assert!(!t.contains("last seen:"), "{t}");
        assert!(!t.contains("no activity"), "{t}");
        assert!(t.contains(hint), "{t}");

        // With silence, no step.
        let t = format_lost_job_text(Some(120), None, &[], hint);
        assert!(t.contains("no activity for 120s"), "{t}");
        assert!(!t.contains("last seen:"), "{t}");

        // With silence and step.
        let t = format_lost_job_text(Some(45), Some("run_command: cargo test (12s ago)"), &[], hint);
        assert!(t.contains("no activity for 45s"), "{t}");
        assert!(
            t.contains("\nlast seen: run_command: cargo test (12s ago)\n"),
            "{t}"
        );
        assert!(t.ends_with(hint), "{t}");

        // No silence, with step.
        let t = format_lost_job_text(None, Some("edit_file: lib.rs (3s ago)"), &[], hint);
        assert!(!t.contains("no activity"), "{t}");
        assert!(
            t.contains("\nlast seen: edit_file: lib.rs (3s ago)\n"),
            "{t}"
        );
    }

    #[test]
    fn format_lost_job_text_lists_written_paths_capped() {
        let hint = "RESUME …";

        // Empty: no line
        let t = format_lost_job_text(None, None, &[], hint);
        assert!(!t.contains("written before the stop"), "{t}");

        // Shows paths
        let paths: Vec<String> = vec!["a.rs".into(), "b.rs".into()];
        let t = format_lost_job_text(None, None, &paths, hint);
        assert!(
            t.contains("\nwritten before the stop (may be half-done): a.rs, b.rs\n"),
            "{t}"
        );

        // Caps at 8 with (+N more)
        let many: Vec<String> = (1..=10).map(|i| format!("f{i}.rs")).collect();
        let t = format_lost_job_text(None, None, &many, hint);
        assert!(
            t.contains("\nwritten before the stop (may be half-done): f1.rs, f2.rs, f3.rs, f4.rs, f5.rs, f6.rs, f7.rs, f8.rs (+2 more)\n"),
            "{t}"
        );
    }

    #[test]
    fn a_waiter_counts_only_while_its_pid_lives() {
        let live = |_: u32| true;
        let dead = |_: u32| false;
        assert!(waiter_pid_is_live("4242\n", &live));
        assert!(!waiter_pid_is_live("4242", &dead));
        assert!(!waiter_pid_is_live("", &live));
        assert!(!waiter_pid_is_live("not a pid", &live));
    }
}
