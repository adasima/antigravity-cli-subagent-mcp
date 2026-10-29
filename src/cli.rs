use std::time::SystemTime;

use crate::agy;
use crate::default_idle_timeout;

const CLI_USAGE: &str = "\
agy-mcp — bridge to Google's Antigravity CLI. With NO arguments it runs as an MCP server (the
invocation Claude Code registers). Subcommands read local state only — no agy launch, no quota:
  agy-mcp wait [<job_id>...] [--all] [--timeout <secs>] [--interval <secs>]
      Block until the job(s) finish, print each final report, exit 0 (all done) / 1 (any failed) /
      2 (timeout or unknown id). Without --timeout it keeps waiting until each job is past its own run
      ceiling (AGY_TIMEOUT, default 3600s, + 600s from its start). --interval 3. Run this as a Claude Code
      background task to await a job with ZERO polling tokens.
      With NO job_id it waits for everything this workspace is still owed — the running jobs plus
      any finished result nobody collected — scoped to the cwd (--all ignores the scope). It waits
      for ALL of them, not the newest: runs are dispatched in parallel, so picking one would let
      several `wait`s watch the same job while the rest ran unwatched.
  agy-mcp poll <job_id>      One-shot status (no blocking).
  agy-mcp jobs               List recent background jobs.
  agy-mcp show <job_id>      Print the saved dispatch spec (full prompt, workspace, conversation
                             id) for a job — how to resume or re-dispatch one that was lost.
  agy-mcp stats [<limit>]    Per-model run summary from ~/.agy-mcp/runs.jsonl: count, ok%,
                             median/p90 steps · ctx · elapsed, requested-vs-served mismatch rate,
                             and the burn-rate estimate. Default limit 200.
  agy-mcp quota              Last known quota per group with its age, plus burn rate — from the
                             local log, not a fresh scrape (that needs an MCP session:
                             agy_diag(action=live), which serializes against running jobs).
  agy-mcp doctor             Why the agy_* tools may be missing: running servers, agy binary,
                             job-mirror state.
  agy-mcp setup [show|apply <posture>|remove]
                             Inspect or change how delegated runs are permitted to act, across
                             the four files that decide it. `show` reads the real state (and
                             names the combinations that look set but do nothing);
                             `apply gate|guarded` is idempotent; `remove` = `apply gate`.
  agy-mcp install-hook [--uninstall]
                             Opt-in: register this binary as agy's PreToolUse guard in
                             ~/.gemini/config/hooks.json. Only does something once the headless
                             gate is off (`toolPermission: always-proceed`); with the gate on,
                             agy soft-denies before the guard is consulted. Prints what else to
                             set. NOT run by install.ps1 — turning it on changes how every agy
                             run on this machine behaves.
  agy-mcp check-command      The guard itself: reads one hook payload on stdin, prints one
                             decision on stdout. Called by agy, not by you.

There is deliberately NO dispatch subcommand: starting a run needs the MCP server (quota,
concurrency, MCP scoping all live there). If the agy_* tools are missing from a session, run
`agy-mcp doctor` — do NOT hand-roll a stdio JSON-RPC client against this binary.";

/// CLI mode: any `agy-mcp <subcommand>` invocation runs a one-shot command and
/// exits (never returns), instead of starting the MCP stdio server (the no-arg
/// path). These only read the disk job mirror, so a separate process — e.g. a
/// Claude Code background task running `agy-mcp wait <id>` — can watch a run to
/// completion without the supervisor paying LLM tokens to poll over MCP.
pub(crate) fn run_cli(sub: &str, rest: &[String]) -> ! {
    // Demote records whose owning server is gone BEFORE reading any of them.
    //
    // This sweep used to run only at server startup, with a comment saying it was
    // there so `agy-mcp wait` couldn't block forever on a job nobody is running —
    // but server startup is exactly when the CLI isn't involved. Caught live
    // 2026-08-14: a probe terminated its server mid-run and `agy-mcp wait j12`
    // then sat on the corpse until its own 1800s timeout. Owner-aware as always
    // (`should_demote_job`), so this can't fail a live sibling's job.
    agy::mark_orphaned_running(&crate::is_pid_alive);
    match sub {
        "wait" => cli_wait(rest),
        "poll" => cli_poll(rest),
        "jobs" => cli_jobs(),
        "show" => cli_show(rest),
        "stats" => cli_stats(rest),
        "quota" => cli_quota(),
        "doctor" => cli_doctor(),
        "setup" => crate::setup::run_setup(rest),
        "check-command" => crate::hookguard::run_check_command(),
        "install-hook" => crate::hookguard::run_install_hook(rest),
        "-h" | "--help" | "help" => {
            println!("{CLI_USAGE}");
            std::process::exit(0);
        }
        other => {
            // Say the likeliest cause out loud. A subcommand the docs describe but this
            // binary has never heard of means the INSTALLED exe is older than the docs,
            // not that the docs are wrong — the running MCP server pins ~/.cargo/bin, so
            // a session that skipped `install.ps1` keeps an old CLI too. Hit live
            // 2026-08-14: `agy-mcp stats` answered "unknown command" while AGENTS.md and
            // the skill both documented it.
            eprintln!(
                "agy-mcp: unknown command '{other}'\n\n\
                 If you expected this subcommand to exist, the INSTALLED binary ({}) is \
                 probably older than the source you are reading. Redeploy with \
                 `install.ps1` and restart Claude Code.\n\n{CLI_USAGE}",
                std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "agy-mcp".to_string())
            );
            std::process::exit(2);
        }
    }
}

/// Pure decision: which workspace an argument-less CLI invocation is scoped to.
///
/// `AGY_WORKSPACE` wins over the cwd because it is an explicit configuration and
/// the cwd is wherever the shell happened to be. The other order is not a style
/// preference — it makes the env var **unreachable**, since `current_dir()`
/// effectively always succeeds, and a `wait` run from the wrong directory then
/// selects nothing and reads as "the job died".
fn resolve_cli_workspace(env_ws: Option<&str>, cwd: Option<String>) -> Option<String> {
    if let Some(w) = env_ws.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(w.to_string());
    }
    cwd
}

/// The workspace an argument-less CLI invocation is about: `AGY_WORKSPACE` if set
/// and non-empty, else the process cwd.
///
/// The CLI speaks no MCP, so it can't ask for the client's roots the way
/// `resolve_workspace` does — but it doesn't need to. The recommended invocation is
/// a Claude Code background task, whose cwd IS the project root the client would
/// have advertised as its root, so the cwd gets the same scope without a server
/// round-trip. `AGY_WORKSPACE` is the explicit override on top of that.
fn cli_scope_workspace() -> Option<String> {
    let env = std::env::var("AGY_WORKSPACE").ok();
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    resolve_cli_workspace(env.as_deref(), cwd)
}

/// Pure decision: what to say when the scoped selection came back empty.
///
/// "Nothing here" and "nothing anywhere" are different situations, and only one of
/// them is the caller's mistake. Naming the count that exists outside the scope is
/// what turns a silent exit 2 into a diagnosis.
fn format_empty_wait_targets(ws: Option<&str>, elsewhere_count: usize) -> String {
    let scope = ws.map(|w| format!(" for {w}")).unwrap_or_default();
    if elsewhere_count > 0 {
        format!(
            "agy-mcp wait: no running or uncollected job{scope} ({elsewhere_count} outstanding \
             elsewhere; pass --all to wait across workspaces). Pass a <job_id> or see \
             `agy-mcp jobs`."
        )
    } else {
        format!(
            "agy-mcp wait: no running or uncollected job{scope}. Pass a <job_id>, add --all \
             to ignore the workspace scope, or see `agy-mcp jobs`."
        )
    }
}

fn cli_wait(rest: &[String]) -> ! {
    let mut job_ids: Vec<String> = Vec::new();
    let mut timeout_s: u64 = 1800;
    let mut explicit_timeout = false;
    let mut interval_s: u64 = 3;
    let mut all = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--timeout" => {
                i += 1;
                timeout_s = rest
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(timeout_s);
                explicit_timeout = true;
            }
            "--interval" => {
                i += 1;
                interval_s = rest
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(interval_s);
            }
            "--all" => all = true,
            "-h" | "--help" => {
                println!("{CLI_USAGE}");
                std::process::exit(0);
            }
            s if !s.starts_with('-') => job_ids.push(s.to_string()),
            // Never ignore an unrecognized flag: with no ids the command falls
            // through to "wait for everything outstanding", so a typo'd flag would
            // silently wait on a different set than the caller asked for.
            other => {
                eprintln!("agy-mcp wait: unknown option '{other}'\n\n{CLI_USAGE}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    // No id given: wait for everything this workspace is still owed — the jobs still
    // running plus any finished result nobody collected. Scoped to the cwd unless
    // --all, since the mirror dir is shared by every session on the machine.
    if job_ids.is_empty() {
        let ws = if all { None } else { cli_scope_workspace() };
        let recs = agy::list_job_records();
        let now = now_unix_cli();
        job_ids = agy::select_wait_targets(&recs, ws.as_deref(), now);
        if job_ids.is_empty() {
            // Under --all the global set was already the query, so there is no
            // "elsewhere" left to report.
            let elsewhere = if ws.is_some() {
                agy::select_wait_targets(&recs, None, now).len()
            } else {
                0
            };
            eprintln!("{}", format_empty_wait_targets(ws.as_deref(), elsewhere));
            std::process::exit(2);
        }
        // Always name the set, even when it's one. The caller asked "wait for
        // whatever is outstanding" and needs to see what that turned out to be —
        // silently watching a different job than intended is the failure mode.
        eprintln!(
            "agy-mcp wait: {} outstanding job(s): {}",
            job_ids.len(),
            job_ids.join(", ")
        );
    }

    // Let agy_poll tell the caller this job is already watched (see `mark_job_waited`).
    for id in &job_ids {
        agy::mark_job_waited(id);
    }
    let interval = std::time::Duration::from_secs(interval_s.max(1));
    // Explicit --timeout: fixed Instant deadline, exactly today's behavior.
    // No --timeout: per-job deadline derived from AGY_TIMEOUT + margin, checked
    // via wall-clock unix time against each job's started_unix. This stops the
    // supervisor re-arming `wait` several times for a healthy long run.
    let fixed_deadline = explicit_timeout
        .then(|| std::time::Instant::now() + std::time::Duration::from_secs(timeout_s));
    let run_ceiling = if explicit_timeout {
        0 // unused
    } else {
        wait_run_ceiling(std::env::var("AGY_TIMEOUT").ok().as_deref())
    };
    // A record is rewritten in place while the job runs (liveness heartbeats), so a
    // read can land mid-write and come back unparseable. Only treat repeated misses
    // as a genuinely unknown id — one transient miss must not abort the wait.
    let mut misses: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut pending = job_ids.clone();
    // Worst outcome across the set: any failure makes the whole wait exit 1, so a
    // caller that only checks the exit code can't mistake a partial success for a
    // clean fanout.
    let mut worst = 0;
    let mut stall_warned: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        pending.retain(|id| {
            match agy::read_job_record(id) {
                Some(rec) if !rec.is_running() => {
                    let body = rec.result.as_deref().unwrap_or("(no output)");
                    if rec.status == "done" {
                        println!("job {id} [{}] done in {}s · model {}:\n\n{body}\n", rec.tool, rec.secs(), rec.model);
                    } else {
                        println!("job {id} [{}] FAILED after {}s:\n{body}\n", rec.tool, rec.secs());
                        worst = worst.max(1);
                    }
                    // Printing IS collection — record it so the server's next
                    // response doesn't warn about a result already handed over.
                    agy::mark_job_seen(id);
                    false
                }
                Some(rec) if agy::should_demote_job(&rec, &crate::is_pid_alive) => {
                    // The owner died WHILE we were waiting — the startup sweep can't
                    // help, and nothing will ever write this record again. Say so and
                    // hand over the recovery spec instead of waiting out the timeout.
                    misses.remove(id);
                    agy::mark_orphaned_running(&crate::is_pid_alive);
                    let hint = agy::read_job_record(id)
                        .and_then(|r| r.result)
                        .unwrap_or_else(|| agy::format_lost_job_text(
                            rec.silent_s(),
                            rec.last_step.as_deref(),
                            &rec.written_paths(),
                            &rec.recovery_hint(),
                        ));
                    println!("job {id} [{}] LOST after {}s:\n{hint}\n", rec.tool, rec.secs());
                    worst = worst.max(1);
                    false
                }
                Some(rec) => {
                    misses.remove(id);
                    // Report a stalled run instead of blocking on it in silence.
                    if let Some(s) = rec.silent_s() {
                        if s > default_idle_timeout() && stall_warned.insert(id.clone()) {
                            eprintln!("agy-mcp wait: job {id} has shown no agy activity for {s}s — it may be stalled.");
                        }
                    }
                    true
                }
                None => {
                    let n = misses.entry(id.clone()).or_insert(0);
                    *n += 1;
                    if *n >= 3 {
                        eprintln!("agy-mcp wait: no job '{id}' on disk (unknown id, or it predates this server / was pruned).");
                        worst = worst.max(2);
                        return false;
                    }
                    true
                }
            }
        });
        if pending.is_empty() {
            exit_wait(&job_ids, worst);
        }
        // Explicit --timeout: single Instant deadline, exactly the old behavior.
        if let Some(dl) = fixed_deadline {
            if std::time::Instant::now() >= dl {
                eprintln!(
                    "agy-mcp wait: still running after {timeout_s}s — gave up on {}. \
                     They may still finish; check `agy-mcp poll <id>`.",
                    pending.join(", ")
                );
                exit_wait(&job_ids, 2);
            }
        } else {
            // Dynamic: give up only when every remaining job has exceeded its
            // own run_ceiling (AGY_TIMEOUT + margin from its start time).
            let now = now_unix_cli();
            let all_expired = pending.iter().all(|id| {
                agy::read_job_record(id)
                    .map(|r| job_past_ceiling(r.started_unix, now, run_ceiling))
                    .unwrap_or(true) // unknown record: don't block on it
            });
            if all_expired {
                eprintln!(
                    "agy-mcp wait: every remaining job exceeded the run ceiling ({run_ceiling}s) — gave up on {}. \
                     They may still finish; check `agy-mcp poll <id>`.",
                    pending.join(", ")
                );
                exit_wait(&job_ids, 2);
            }
        }
        std::thread::sleep(interval);
    }
}

/// Margin past a run's own hard ceiling before a default `wait` gives up. Covers
/// what happens after agy exits: draining backgrounded tasks, the verify command.
const WAIT_CEILING_MARGIN_S: u64 = 600;

/// How long a default `wait` (no `--timeout`) keeps watching a job, measured from
/// the job's own start. A fixed 1800s gave up on healthy runs the server was still
/// allowed to finish (its ceiling is AGY_TIMEOUT, 3600s by default), so the
/// supervisor had to re-arm `wait` for every long job.
/// Leave `agy-mcp wait`, dropping the watched marks first: a later agy_poll must not
/// tell the caller a wait is watching once it has gone.
fn exit_wait(job_ids: &[String], code: i32) -> ! {
    for id in job_ids {
        agy::clear_job_waited(id);
    }
    std::process::exit(code);
}

fn wait_run_ceiling(agy_timeout_env: Option<&str>) -> u64 {
    crate::timeout_from_env_str(agy_timeout_env, crate::SUB_TIMEOUT_S) + WAIT_CEILING_MARGIN_S
}

fn job_past_ceiling(started_unix: u64, now_unix: u64, ceiling_s: u64) -> bool {
    now_unix.saturating_sub(started_unix) >= ceiling_s
}

/// Wall-clock unix seconds for the CLI paths (the agy module keeps its own).
fn now_unix_cli() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cli_poll(rest: &[String]) -> ! {
    let Some(job_id) = rest.first() else {
        eprintln!("agy-mcp poll: missing <job_id>\n\n{CLI_USAGE}");
        std::process::exit(2);
    };
    match agy::read_job_record(job_id) {
        Some(rec) => {
            print!(
                "{}",
                crate::format_job_detail(
                    job_id,
                    &rec.tool,
                    &rec.model,
                    &rec.status,
                    rec.secs(),
                    rec.result.as_deref(),
                    &rec.prompt_head,
                    rec.silent_s(),
                    rec.last_step.as_deref(),
                    agy::job_is_waited(job_id, &crate::is_pid_alive),
                )
            );
            println!();
            // A terminal result printed here has been collected (see JobRecord::seen).
            agy::mark_job_seen(job_id);
            std::process::exit(if rec.status == "failed" { 1 } else { 0 });
        }
        None => {
            eprintln!("agy-mcp poll: no job '{job_id}' on disk.");
            std::process::exit(2);
        }
    }
}

/// `agy-mcp stats [<limit>]` — the run-log summary that used to be
/// `agy_diag(action=stats)`.
///
/// It moved off the MCP schema because that schema is loaded into EVERY session
/// whether or not anyone looks at stats, while the answer is a pure read of
/// `~/.agy-mcp/runs.jsonl` that any process can do. The knowledge stays available;
/// only the standing cost is gone.
fn cli_stats(rest: &[String]) -> ! {
    let limit = rest.first().and_then(|s| s.parse::<usize>().ok());
    println!("{}", agy::run_stats(Some(limit.unwrap_or(200))));
    std::process::exit(0);
}

/// `agy-mcp quota` — the accrued quota picture, local files only. The LIVE scrape
/// deliberately stays an MCP action: it drives a full interactive agy and has to
/// be serialized against running jobs, which only the server can do.
fn cli_quota() -> ! {
    println!("{}", agy::cached_quota_report());
    std::process::exit(0);
}

fn cli_jobs() -> ! {
    let recs = agy::list_job_records();
    if recs.is_empty() {
        println!("no background jobs on disk.");
        std::process::exit(0);
    }
    println!("background jobs ({}):", recs.len());
    for r in &recs {
        let silent = r
            .silent_s()
            .filter(|s| *s > default_idle_timeout())
            .map(|s| format!(" ⚠ silent {s}s"))
            .unwrap_or_default();
        // Only meaningful for a still-running job — a done/failed record's
        // last_step is whatever it was doing right before it stopped, which the
        // result line above already supersedes.
        let now_step = r
            .is_running()
            .then_some(r.last_step.as_deref())
            .flatten()
            .map(|s| format!(" · now: {s}"))
            .unwrap_or_default();
        println!(
            "  {} [{}] {} {}s · model {}{silent} · \"{}\"{now_step}",
            r.id,
            r.tool,
            r.status,
            r.secs(),
            r.model,
            r.prompt_head
        );
    }
    std::process::exit(0);
}

/// Print a job's saved dispatch spec. This is the recovery path for a run that
/// died with its supervisor: the prompt is the expensive artifact, and it's on
/// disk, so resuming or re-dispatching costs no tokens to reconstruct.
fn cli_show(rest: &[String]) -> ! {
    let Some(job_id) = rest.first() else {
        eprintln!("agy-mcp show: missing <job_id>\n\n{CLI_USAGE}");
        std::process::exit(2);
    };
    let Some(rec) = agy::read_job_record(job_id) else {
        eprintln!("agy-mcp show: no job '{job_id}' on disk.");
        std::process::exit(2);
    };
    println!(
        "job {} [{}] {} · model {} · {}s",
        rec.id,
        rec.tool,
        rec.status,
        rec.model,
        rec.secs()
    );
    println!("workspace: {}", rec.workspace);
    if !rec.add_dirs.is_empty() {
        println!("add_dirs: {}", rec.add_dirs.join(", "));
    }
    if !rec.preset.is_empty() {
        println!("preset: {}", rec.preset);
    }
    println!(
        "conversation_id: {}",
        rec.conversation_id.as_deref().unwrap_or("(none recorded)")
    );
    if let Some(s) = rec.silent_s() {
        println!("last agy activity: {s}s ago");
    }
    // Only a FAILED job needs recovering; printing this for a finished one would
    // just invite a pointless resume.
    if rec.status == "failed" {
        println!("\n--- recovery ---\n{}", rec.recovery_hint());
    }
    if rec.prompt.is_empty() {
        println!("\n--- prompt ---\n(not saved — this record predates dispatch-spec persistence)");
    } else {
        println!("\n--- prompt ---\n{}", rec.prompt);
    }
    std::process::exit(0);
}

/// Explain the state of the bridge itself. Exists so a session whose `agy_*`
/// tools are missing has a sanctioned way to find out WHY — improvising a stdio
/// JSON-RPC client against this binary (observed) tells you nothing and risks
/// leaving a stray server around.
fn cli_doctor() -> ! {
    println!("agy binary: {}", agy::agy_bin());
    println!(
        "this exe:   {}",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    let servers = agy::live_server_pids(&crate::is_pid_alive);
    if servers.is_empty() {
        println!("running agy-mcp servers: none");
        println!("  → no session currently has the agy_* tools. Restart Claude Code (the MCP");
        println!("    server is spawned per session), or check `claude mcp list`.");
    } else {
        println!(
            "running agy-mcp servers: {}",
            servers
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("  → servers are per-session and may coexist; each session spawns its own.");
    }
    let recs = agy::list_job_records();
    let running = recs.iter().filter(|r| r.is_running()).count();
    let stalled = recs
        .iter()
        .filter(|r| r.silent_s().is_some_and(|s| s > default_idle_timeout()))
        .count();
    println!(
        "jobs on disk: {} ({running} running, {stalled} silent past the idle timeout)",
        recs.len()
    );
    for r in recs.iter().filter(|r| r.is_running()) {
        println!("  {} [{}] {}s · {}", r.id, r.tool, r.secs(), r.prompt_head);
    }
    println!("{}", crate::default_model_line(crate::default_model_env()));
    println!("\nDispatch is MCP-only — see `agy-mcp --help`.");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_wait_follows_the_run_ceiling() {
        assert_eq!(wait_run_ceiling(None), 3600 + WAIT_CEILING_MARGIN_S);
        assert_eq!(wait_run_ceiling(Some("7200")), 7200 + WAIT_CEILING_MARGIN_S);
        // Same parsing as the server: junk or 0 falls back to the default.
        assert_eq!(wait_run_ceiling(Some("0")), 3600 + WAIT_CEILING_MARGIN_S);
        assert!(!job_past_ceiling(1_000, 1_000 + 4_199, 4_200));
        assert!(job_past_ceiling(1_000, 1_000 + 4_200, 4_200));
        // A start time in the future (clock skew) is not past anything.
        assert!(!job_past_ceiling(2_000, 1_000, 4_200));
    }

    /// The env var used to sit BELOW the cwd, which made it dead code.
    #[test]
    fn an_explicit_agy_workspace_beats_whatever_directory_the_shell_was_in() {
        assert_eq!(
            resolve_cli_workspace(Some("/explicit/root"), Some("/some/cwd".to_string())).as_deref(),
            Some("/explicit/root")
        );
    }

    #[test]
    fn an_unset_or_blank_env_var_falls_back_to_the_cwd() {
        let cwd = || Some("/some/cwd".to_string());
        assert_eq!(
            resolve_cli_workspace(None, cwd()).as_deref(),
            Some("/some/cwd")
        );
        assert_eq!(
            resolve_cli_workspace(Some(""), cwd()).as_deref(),
            Some("/some/cwd")
        );
        assert_eq!(
            resolve_cli_workspace(Some("   \t  "), cwd()).as_deref(),
            Some("/some/cwd")
        );
    }

    /// A `wait` that finds nothing has to say WHICH nothing it found: an empty
    /// machine, or a job sitting in another workspace. The second one is why a
    /// field report read as "the job died" when it had not.
    #[test]
    fn an_empty_selection_names_the_jobs_outstanding_elsewhere() {
        let here = format_empty_wait_targets(Some("/proj"), 0);
        assert!(
            here.contains("no running or uncollected job for /proj."),
            "{here}"
        );
        assert!(!here.contains("elsewhere"), "{here}");

        let there = format_empty_wait_targets(Some("/proj"), 3);
        assert!(there.contains("3 outstanding elsewhere"), "{there}");
        assert!(there.contains("--all"), "{there}");

        // No scope (--all): nothing to say about "elsewhere".
        let all = format_empty_wait_targets(None, 0);
        assert!(
            all.starts_with("agy-mcp wait: no running or uncollected job."),
            "{all}"
        );
    }
}
