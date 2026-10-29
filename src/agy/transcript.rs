use crate::agy::paths::{brain_dir, transcript_path};
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A background task (e.g. `cargo build` agy shelled out and WAITs on) that
/// reported completion via a `SYSTEM_MESSAGE` after agy ended its turn.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskResult {
    /// agy task id, e.g. "<conv>/task-18".
    pub id: String,
    /// Whether the command reported success.
    pub ok: bool,
    /// The process exit code if known.
    pub exit_code: Option<i32>,
    /// The detected test runner summary if any.
    pub summary: Option<String>,
    /// A short, cleaned tail of the command output (token-frugal).
    pub tail: String,
    /// The command line that launched it, when the transcript shows one — what
    /// "the same command" means when a sweep reruns it (`dedupe_task_results`).
    pub cmd: Option<String>,
}

/// Structured read of a conversation transcript.
pub struct Transcript {
    /// The final report (terminal PLANNER_RESPONSE, no trailing tool call).
    pub answer: String,
    /// Model agy reported actually serving (from the settings-change receipt).
    pub served_model: Option<String>,
    /// Files the sub-agent created/modified (paths only — never the bodies).
    pub artifacts: Vec<String>,
    /// One-line tool-call tally, e.g. "6 steps: 3×run_command, 1×write_to_file".
    pub step_summary: String,
    /// SYSTEM ERROR_MESSAGE entries seen (e.g. 429 "model API ... overloaded"),
    /// in order. Distinct from a trust/OAuth-prompt stall: these are receipts
    /// agy itself logged, so we can name the real cause instead of guessing.
    pub errors: Vec<String>,
    /// Background tasks LAUNCHED (RUN_COMMAND "running as a background task") but
    /// not yet matched by a completion SYSTEM_MESSAGE — i.e. agy ended its turn
    /// while a `cargo build` it shelled out was still running. run_agy keeps
    /// holding the job until this is empty (or the timeout caps fire).
    pub pending_tasks: Vec<String>,
    /// Background tasks that DID report completion (success/failure + output tail).
    /// agy's re-ignited turn after a build is typically empty, so this SYSTEM_MESSAGE
    /// is the source of truth for the build outcome — folded into the answer.
    pub task_results: Vec<TaskResult>,
    /// How many consecutive ERROR_MESSAGE entries at the END of the transcript
    /// are quota walls (is_quota_wall). Resets to 0 on any other entry.
    pub trailing_quota_retries: usize,
}

/// agy tool-call args are double-encoded (each value is itself a JSON string);
/// decode one layer to recover the real path/command. Falls back to trimming.
pub(crate) fn decode_arg(v: &serde_json::Value) -> Option<String> {
    let s = v.as_str()?;
    Some(serde_json::from_str::<String>(s).unwrap_or_else(|_| s.trim_matches('"').to_string()))
}

/// "file:///C:/Users/..." -> "C:\Users\..." (Windows-targeted).
fn norm_file_url(p: &str) -> String {
    let s = p.trim();
    let s = s.strip_prefix("file://").unwrap_or(s);
    let b = s.as_bytes();
    let s = if b.len() >= 3 && b[0] == b'/' && b[2] == b':' {
        &s[1..]
    } else {
        s
    };
    s.replace('/', "\\")
}

/// Collapse whitespace and clip to `n` chars (char-safe).
pub(crate) fn clip(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n {
        s
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

/// Strip agy's "Created At:/Completed At:/Using prompt:" preamble from a result.
fn strip_boiler(c: &str) -> String {
    c.lines()
        .map(|l| l.trim())
        .filter(|t| {
            !t.is_empty()
                && !t.starts_with("Created At:")
                && !t.starts_with("Completed At:")
                && !t.starts_with("Using prompt:")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns the first non-empty line of content after skipping agy header lines.
/// Without this, a GENERIC entry (used for view_file/grep as well as receipts)
/// matching a receipt phrase anywhere inside viewed file text would create fake
/// write receipts or fake pending tasks.
fn first_meaningful_line(content: &str) -> Option<&str> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("Created At:")
            || trimmed.starts_with("Completed At:")
            || trimmed.starts_with("Using prompt:")
        {
            continue;
        }
        return Some(trimmed);
    }
    None
}

/// A RUN_COMMAND result for a long command is reported as a background task:
/// "...running as a background task with task id: <conv>/task-N ...". Extract
/// the task id (agy then ends its turn while the command keeps running).
fn parse_bg_launch(content: &str) -> Option<String> {
    let rest = content
        .split("running as a background task with task id:")
        .nth(1)?;
    let id = rest.split_whitespace().next()?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

fn parse_exit_code(body: &str) -> Option<i32> {
    if body.contains("completed successfully") {
        return Some(0);
    }
    let lower = body.to_lowercase();
    // "exited with code" comes first and is not a typo for "exit code": it is agy's
    // own wording (observed 2026-08-14), and none of the three keys below match it.
    for key in &[
        "exited with code",
        "exited with status",
        "exit code",
        "exit status",
        "status",
    ] {
        if let Some(idx) = lower.find(key) {
            let after = &lower[idx + key.len()..];
            let num_str: String = after
                .chars()
                .skip_while(|c| c.is_whitespace() || *c == ':' || *c == '=')
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !num_str.is_empty() {
                if let Ok(code) = num_str.parse::<i32>() {
                    return Some(code);
                }
            }
        }
    }
    None
}

/// The path out of an edit receipt's tail — everything after `tool to: ` in
/// "The following changes were made by the (multi_)replace_file_content tool to:
/// `<path>`…". Pure.
///
/// **Take the first line before anything else.** agy has two shapes for this: one
/// ends the sentence with `. If relevant, …`, the other ends the line with a colon
/// and then dumps the diff. Splitting on the sentence marker alone returns the
/// whole remainder when it is absent, so a multi-line diff block plus its trailing
/// notice was pushed in as one "path" and printed in `wrote:` (observed
/// 2026-08-15). A path never spans a newline, so the line bound is the real one.
fn edit_receipt_path(rest: &str) -> Option<String> {
    let path = rest
        .lines()
        .next()
        .unwrap_or(rest)
        .split(". If relevant")
        .next()
        .unwrap_or(rest)
        .trim()
        .trim_end_matches(':')
        .trim_matches('`')
        .trim_end_matches('.')
        .trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// agy 1.4.x's `edit_file` receipt:
/// `Made 2 replacements in G:\repo\src\x.rs starting at line 12 (+1/-0 lines).`
fn replacement_receipt_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("Made ")?;
    let (count, rest) = rest.split_once(" in ")?;
    if !count.contains("replacement") {
        return None;
    }
    let path = rest.split(" starting at line").next()?.trim();
    (!path.is_empty()).then(|| norm_file_url(path))
}

/// The `Path` argument of a `write_file` call. agy stores each argument as a
/// JSON-encoded string (`"\"G:\\\\repo\\\\a.txt\""`), so it is decoded once more.
fn write_call_path(tc: &serde_json::Value) -> Option<String> {
    let raw = tc.get("args")?.get("Path")?.as_str()?;
    let path = serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string());
    let path = path.trim();
    (!path.is_empty()).then(|| norm_file_url(path))
}

/// The test runner's summary line, for the footer's task result.
///
/// `cargo test` prints one `test result:` line per test binary (unit, each
/// integration file, doc-tests), and the last one is usually the doc-tests'
/// `0 passed`. Quoting only that one is how a footer said nothing useful while the
/// report claimed "74 passed" against a real 111–126 (gen3dasset 2026-09-22) — so
/// several cargo lines are summed into one, naming how many suites it covers.
pub(crate) fn find_test_summary(text: &str) -> Option<String> {
    if let Some(sum) = sum_cargo_test_results(text) {
        return Some(sum);
    }
    for line in text.lines().rev() {
        let trimmed = line.trim();
        if trimmed.contains("test result:")
            && (trimmed.contains("passed;") || trimmed.contains("failed;"))
        {
            return Some(trimmed.to_string());
        }
        let lower = trimmed.to_lowercase();
        if (lower.starts_with("tests:")
            || lower.starts_with("tests ")
            || lower.starts_with("test files:"))
            && (lower.contains("failed") || lower.contains("passed") || lower.contains("total"))
        {
            return Some(trimmed.to_string());
        }
        if trimmed.starts_with("===")
            && trimmed.ends_with("===")
            && (trimmed.contains("passed") || trimmed.contains("failed"))
        {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Sum two or more cargo `test result:` lines into one; None for fewer (the
/// single line is quoted as-is by [`find_test_summary`]).
fn sum_cargo_test_results(text: &str) -> Option<String> {
    let count = |line: &str, what: &str| -> Option<u64> {
        let idx = line.find(&format!(" {what}"))?;
        line[..idx].rsplit([' ', '.', ';']).next()?.parse().ok()
    };
    let (mut suites, mut passed, mut failed, mut ignored) = (0u32, 0u64, 0u64, 0u64);
    for line in text.lines().map(str::trim) {
        if !line.starts_with("test result:") {
            continue;
        }
        let (Some(p), Some(f)) = (count(line, "passed;"), count(line, "failed;")) else {
            continue;
        };
        suites += 1;
        passed += p;
        failed += f;
        ignored += count(line, "ignored").unwrap_or(0);
    }
    (suites > 1).then(|| {
        let verdict = if failed > 0 { "FAILED" } else { "ok" };
        format!(
            "test result: {verdict}. {passed} passed; {failed} failed; {ignored} ignored \
             (sum of {suites} suites)"
        )
    })
}

/// When a background task finishes, agy appends a SYSTEM_MESSAGE:
/// "...sender=<conv>/task-N ... content=Task id \"<conv>/task-N\" finished with
/// result: <body>". Extract (id, ok, exit_code, summary, short tail).
// The tuple IS the parse result; naming it would add a type no second call site constructs.
#[allow(clippy::type_complexity)]
fn parse_bg_finish(content: &str) -> Option<(String, bool, Option<i32>, Option<String>, String)> {
    let (head, body) = content.split_once("finished with result:")?;

    // id from `sender=<id>` (bounded by whitespace), the reliable token.
    let id = head
        .split_once("sender=")?
        .1
        .split_whitespace()
        .next()?
        .to_string();

    let exit_code = parse_exit_code(body);
    let summary = find_test_summary(body);
    let ok = bg_finish_ok(body, exit_code, summary.as_deref());
    let tail = clip(body, 220);

    Some((id, ok, exit_code, summary, tail))
}

/// Did the receipt report success? Pure so the wording can be pinned by a test.
///
/// agy words this at least two ways — `The command completed successfully.` and
/// `The command exited with code 0.` (the latter observed 2026-08-14). Deciding
/// on the phrase alone made the second wording read as a failure: a `cargo test`
/// with 112 passed and exit 0 was folded into the answer as `✗ task-86: FAILED`,
/// with the receipt quoted on the same line contradicting the verdict. The exit
/// code and the test summary are already parsed right above; they decide, and
/// the phrase is only the last resort for a wording we have not seen.
fn bg_finish_ok(body: &str, exit_code: Option<i32>, summary: Option<&str>) -> bool {
    // A failing test count outranks a zero exit: they normally agree, and when
    // they don't, the count is the one that names an actual defect.
    if let Some((failed, _)) = summary.and_then(parse_test_summary_failed_count) {
        if failed > 0 {
            return false;
        }
    }
    match exit_code {
        Some(code) => code == 0,
        None => body.contains("completed successfully"),
    }
}

/// The model label in a settings-change receipt:
/// "...changed setting `Model Selection` from None to <LABEL>. ..."
fn model_selection_label(content: &str) -> Option<String> {
    let rest = &content[content.find("`Model Selection` from ")?..];
    let after = &rest[rest.find(" to ")? + 4..];
    // End at sentence break ". " — NOT the first '.', since labels
    // like "Gemini 3.5 Flash (High)" contain a dot in the version.
    let end = after.find(". ").unwrap_or(after.len());
    let label = after[..end].trim_end_matches('.').trim();
    crate::agy::models::is_plausible_model_label(label).then(|| label.to_string())
}

/// Parse the transcript: return the model's answer plus the served-model label
/// (from the injected `<USER_SETTINGS_CHANGE>` line, our receipt for which model
/// actually ran).
/// Parse the transcript into answer + served model + artifacts + step tally.
pub(crate) fn parse_full(raw: &str) -> Transcript {
    let mut terminal: Vec<String> = Vec::new(); // planner responses with NO tool call
    let mut any_planner: Vec<String> = Vec::new(); // fallback
    let mut served: Option<String> = None;
    let mut artifacts: Vec<String> = Vec::new();
    let mut tool_counts: std::collections::BTreeMap<String, usize> = Default::default();
    let mut errors: Vec<String> = Vec::new();
    // Background tasks: ids launched, and completions keyed by id (last wins).
    let mut launched: Vec<String> = Vec::new();
    let mut finished: std::collections::BTreeMap<String, TaskResult> = Default::default();
    let mut trailing_quota_retries: usize = 0;
    // agy 1.4.x's `write_file` receipt is a bare "Created file." with no path, so
    // the path comes from the call that asked for it, in the step just before.
    let mut pending_writes: std::collections::VecDeque<String> = Default::default();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let content = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
        let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");

        // Keep the LAST receipt (not the first) — a resume that switched models has
        // several, and the latest is the model the conversation is now on.
        if let Some(label) = model_selection_label(content) {
            served = Some(label);
        }

        // Tool calls: tally only. Artifacts are taken from the *result* entries
        // below (CODE_ACTION / GENERATE_IMAGE) — those are success receipts, so
        // we avoid double-counting the paired write_to_file and any failed write.
        let tool_calls = v.get("tool_calls").and_then(|x| x.as_array());
        if let Some(tcs) = tool_calls {
            pending_writes.clear();
            for tc in tcs {
                if let Some(name) = tc.get("name").and_then(|x| x.as_str()) {
                    *tool_counts.entry(name.to_string()).or_default() += 1;
                    if name == "write_file" {
                        if let Some(p) = write_call_path(tc) {
                            pending_writes.push_back(p);
                        }
                    }
                }
            }
        }

        // SYSTEM ERROR_MESSAGE receipts (e.g. 429 rate-limit/quota-overload).
        if typ == "ERROR_MESSAGE" {
            let err = v.get("error").and_then(|x| x.as_str()).unwrap_or("");
            let code = v.get("error_code").and_then(|x| x.as_i64());
            let line = match code {
                Some(c) => format!("[{c}] {err}"),
                None => err.to_string(),
            };
            if !err.is_empty() && errors.last() != Some(&line) {
                errors.push(line.clone());
            }
            // `line` already carries `err` (with an optional "[code] " prefix), so
            // checking it alone covers both; a bare 429 with no `error` text at all
            // is not a receipt this bridge can act on.
            trailing_quota_retries = if crate::agy::quota::is_quota_wall(&line) {
                trailing_quota_retries + 1
            } else {
                0
            };
        } else {
            trailing_quota_retries = 0;
        }

        // Artifacts from result entries.
        // agy 1.2.x writes edit and create receipts as GENERIC instead of CODE_ACTION.
        // Older transcripts use CODE_ACTION. Accept both by content shape.
        if typ == "GENERATE_IMAGE" {
            if let Some(p) = content.split("saved at ").nth(1) {
                let p = p
                    .lines()
                    .next()
                    .unwrap_or(p)
                    .trim()
                    .trim_end_matches('.')
                    .trim();
                if !p.is_empty() {
                    artifacts.push(p.to_string());
                }
            }
        }
        if typ == "CODE_ACTION" {
            if let Some(rest) = content.split("Created file ").nth(1) {
                let url = rest.split(" with").next().unwrap_or(rest).trim();
                if !url.is_empty() {
                    artifacts.push(norm_file_url(url));
                }
            } else if let Some(rest) = content.split("tool to: ").nth(1) {
                if let Some(path) = edit_receipt_path(rest) {
                    artifacts.push(path);
                }
            }
        } else if typ == "GENERIC" {
            if let Some(first) = first_meaningful_line(content) {
                if first == "Created file." {
                    if let Some(p) = pending_writes.pop_front() {
                        artifacts.push(p);
                    }
                } else if let Some(p) = replacement_receipt_path(first) {
                    artifacts.push(p);
                } else if first.starts_with("Created file ") {
                    if let Some(rest) = content.split("Created file ").nth(1) {
                        let url = rest.split(" with").next().unwrap_or(rest).trim();
                        if !url.is_empty() {
                            artifacts.push(norm_file_url(url));
                        }
                    }
                } else if first.starts_with("The following changes were made by") {
                    if let Some(rest) = content.split("tool to: ").nth(1) {
                        if let Some(path) = edit_receipt_path(rest) {
                            artifacts.push(path);
                        }
                    }
                }
            }
        }

        // Background-task tracking. A long RUN_COMMAND is reported as a launched
        // background task; its completion arrives later as a SYSTEM_MESSAGE (often
        // AFTER agy ended its turn — the re-ignited planner turn is usually empty,
        // so this completion is the real build outcome).
        // In agy 1.2.x, background launches are typed GENERIC instead of RUN_COMMAND;
        // only treat GENERIC as a launch when its first meaningful line begins with
        // "Tool is running as a background task" to avoid matching viewed file content.
        let is_launch = if typ == "RUN_COMMAND" {
            true
        } else if typ == "GENERIC" {
            first_meaningful_line(content)
                .is_some_and(|l| l.starts_with("Tool is running as a background task"))
        } else {
            false
        };
        if is_launch {
            if let Some(id) = parse_bg_launch(content) {
                if !launched.contains(&id) {
                    launched.push(id);
                }
            }
        }
        if typ == "SYSTEM_MESSAGE" {
            if let Some((id, ok, exit_code, summary, tail)) = parse_bg_finish(content) {
                finished.insert(
                    id.clone(),
                    TaskResult {
                        id,
                        ok,
                        exit_code,
                        summary,
                        tail,
                        cmd: None,
                    },
                );
            }
        }

        // Answer collection: the terminal planner response (no tool call) is the report.
        let is_planner_done = v.get("source").and_then(|x| x.as_str()) == Some("MODEL")
            && v.get("status").and_then(|x| x.as_str()) == Some("DONE")
            && typ == "PLANNER_RESPONSE";
        if is_planner_done && !content.is_empty() {
            any_planner.push(content.to_string());
            if tool_calls.map(|a| a.is_empty()).unwrap_or(true) {
                terminal.push(content.to_string());
            }
        }
    }

    let answer = terminal
        .last()
        .cloned()
        .unwrap_or_else(|| any_planner.join("\n"));

    // Drop agy's own bookkeeping files under brain/ (e.g. implementation_plan.md,
    // task.md) — they're agy's scratch notes, not real task output, and would
    // make a punt (no real artifact) look like it produced something. Still
    // visible via agy_trace (reads the transcript directly), just not counted
    // as a deliverable here.
    let brain = format!(
        "file://{}/",
        brain_dir().to_string_lossy().replace('\\', "/")
    )
    .to_lowercase();
    let brain_win = brain_dir().to_string_lossy().to_lowercase();
    artifacts.retain(|a| {
        !a.to_lowercase().starts_with(&brain)
            && !a.to_lowercase().starts_with(&brain_win)
            && !a.to_lowercase().contains("brain/task.md")
            && !a.to_lowercase().contains("brain\\task.md")
    });

    // De-dup artifacts, preserve first-seen order.
    let mut seen = std::collections::HashSet::new();
    artifacts.retain(|a| {
        if seen.contains(a) {
            false
        } else {
            seen.insert(a.clone());
            true
        }
    });

    let step_summary = if tool_counts.is_empty() {
        String::new()
    } else {
        let total: usize = tool_counts.values().sum();
        let parts: Vec<String> = tool_counts
            .iter()
            .map(|(k, n)| format!("{n}×{k}"))
            .collect();
        let s = if total == 1 { "" } else { "s" };
        format!("{total} step{s}: {}", parts.join(", "))
    };

    let pending_tasks: Vec<String> = launched
        .into_iter()
        .filter(|id| !finished.contains_key(id))
        .collect();
    let task_results: Vec<TaskResult> = finished.into_values().collect();

    Transcript {
        answer,
        served_model: served,
        artifacts,
        step_summary,
        errors,
        pending_tasks,
        task_results,
        trailing_quota_retries,
    }
}

pub(crate) fn parse_test_summary_failed_count(summary: &str) -> Option<(usize, bool)> {
    let lower = summary.to_lowercase();

    // Scan all occurrences of "failed" in lower and pick the first one with digits immediately preceding it.
    for (idx, _) in lower.match_indices("failed") {
        let before = lower[..idx].trim_end();
        let num_str: String = before
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if !num_str.is_empty() {
            if let Ok(val) = num_str.parse::<usize>() {
                let has_passed_or_total = lower.contains("passed") || lower.contains("total");
                return Some((val, has_passed_or_total));
            }
        }
    }

    if lower.contains("passed") || lower.contains("total") {
        // No "failed" preceded by digits found, but passed/total present -> 0 failed
        return Some((0, true));
    }

    None
}

pub(crate) fn analyze_task_log_content(
    content: &str,
    summary: Option<String>,
) -> Option<(bool, Option<i32>)> {
    if let Some(ref sum) = summary {
        if let Some((failed_count, is_complete)) = parse_test_summary_failed_count(sum) {
            if failed_count > 0 {
                return Some((false, Some(1)));
            } else if is_complete {
                return Some((true, Some(0)));
            }
        }
    }

    if content.contains("error[") || content.contains("\nerror:") {
        Some((false, Some(1)))
    } else if content.contains("Finished `") {
        Some((true, Some(0)))
    } else {
        None
    }
}

/// Fallback ground truth for a background task's completion when agy never
/// appends the SYSTEM_MESSAGE receipt. Observed empirically (2026-06-25 smoke):
/// agy can background a `cargo build`, finish it cleanly on disk, and never
/// write the completion record at all — the SYSTEM_MESSAGE pairing in
/// `parse_full` then waits forever for something that's never coming. The
/// task's own log (`.system_generated/tasks/<short-id>.log`, written by the
/// shelled-out command itself) is the actual source of truth; look for
/// cargo's terminal markers directly, or non-cargo test summary markers.
pub(crate) fn read_task_log_result(conv_dir: &Path, task_id: &str) -> Option<TaskResult> {
    let short = task_id.rsplit('/').next().unwrap_or(task_id);
    let log = conv_dir
        .join(".system_generated")
        .join("tasks")
        .join(format!("{short}.log"));
    let content = std::fs::read_to_string(&log).ok()?;
    let tail: String = content
        .chars()
        .rev()
        .take(400)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let summary = find_test_summary(&content);
    let (ok, exit_code) = analyze_task_log_content(&content, summary.clone())?;
    Some(TaskResult {
        id: task_id.to_string(),
        ok,
        exit_code,
        summary,
        tail: clip(&tail, 220),
        cmd: None,
    })
}

/// Task id → the `run_command` line that launched it. A launch step follows the
/// planner step carrying its `run_command` call, so the last command seen before a
/// launch is that launch's command. Pure.
pub(crate) fn launch_commands(raw: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut last_cmd: Option<String> = None;
    for line in raw.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if let Some(tcs) = v.get("tool_calls").and_then(|x| x.as_array()) {
            for tc in tcs {
                if tc.get("name").and_then(|x| x.as_str()) == Some("run_command") {
                    if let Some(c) = tc
                        .get("args")
                        .and_then(|a| a.get("CommandLine"))
                        .and_then(decode_arg)
                    {
                        last_cmd = Some(c);
                    }
                }
            }
        }
        let content = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
        let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let is_launch = typ == "RUN_COMMAND"
            || (typ == "GENERIC"
                && first_meaningful_line(content)
                    .is_some_and(|l| l.starts_with("Tool is running as a background task")));
        if is_launch {
            if let (Some(id), Some(c)) = (parse_bg_launch(content), last_cmd.clone()) {
                out.insert(id, c);
            }
        }
    }
    out
}

/// Read the model's answer (and served-model label) from a conversation.
/// Thin wrapper over [`read_full`] kept for callers that only need those two.
pub fn read_transcript(conv_dir: &Path) -> Result<(String, Option<String>)> {
    let t = read_full(conv_dir)?;
    Ok((t.answer, t.served_model))
}

/// Read the model label actually served in a conversation, even if the run
/// produced no final answer (from the settings-change receipt in `partial`).
pub fn read_conv_model(conv_dir: &Path) -> Option<String> {
    match read_full(conv_dir) {
        Ok(t) => t.served_model,
        Err(e) => e
            .downcast_ref::<NoAnswerError>()
            .and_then(|n| n.partial.served_model.clone()),
    }
}

/// Full structured read of a conversation's transcript.jsonl. Errors if the file
/// is missing; if it parsed but yielded no answer, surfaces agy's own logged
/// ERROR_MESSAGE receipt (429 = quota/rate-limit) instead of guessing a cause.
/// A transcript with no final answer — which can still hold everything the run did.
///
/// `read_full` used to say this with a bare `anyhow!` string, which threw the parsed
/// transcript away, so `progress_note` had nothing to report and a run cut after 48
/// `run_command` steps came back as a bare `agy exited with Some(1)` (2026-09-11).
/// The error carries `partial` so the failure report can still say what got done.
pub(crate) struct NoAnswerError {
    pub partial: Transcript,
    pub msg: String,
}
impl std::fmt::Display for NoAnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}
impl std::fmt::Debug for NoAnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}
impl std::error::Error for NoAnswerError {}

fn step_index_of(line: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(line.trim())
        .ok()?
        .get("step_index")?
        .as_i64()
}

/// Highest `step_index` in a raw transcript; None when no line carries one.
pub(crate) fn last_step_index(raw: &str) -> Option<i64> {
    raw.lines().filter_map(step_index_of).max()
}

/// A conversation's last step before a resume is spawned — the line between the
/// earlier turns and the steps the resumed run writes itself. None when there is
/// no transcript (or no step in it), which reads as "no baseline": the whole
/// transcript, the same as a fresh run.
pub fn step_baseline(conv_dir: &Path) -> Option<i64> {
    let raw = std::fs::read_to_string(transcript_path(conv_dir)).ok()?;
    last_step_index(&raw)
}

/// The part of a raw transcript written after `baseline`: from the first line
/// whose `step_index` is past it to the end. `None` is the whole transcript.
///
/// A resume appends to the conversation it continues, so its transcript still
/// ends with the previous turn until agy appends the new USER_INPUT — and agy
/// writes the SQLite WAL / brain subtree (our liveness signal) before that. A
/// previous turn that died on a quota wall therefore ended with three 429s that
/// read as THIS run's trailing retries: the heartbeat killed a resume on another
/// model before it was ever called, returned the old failure verbatim, and logged
/// a false 0% receipt for the new model's group (2026-09-26, j63).
///
/// Scanned from the end: the steps past the baseline are the tail, so a long
/// conversation costs only the new lines on every heartbeat tick.
pub(crate) fn steps_after(raw: &str, baseline: Option<i64>) -> &str {
    let Some(base) = baseline else {
        return raw;
    };
    let mut cut: Option<usize> = None;
    let mut end = raw.len();
    loop {
        let begin = raw[..end].rfind('\n').map_or(0, |i| i + 1);
        match step_index_of(&raw[begin..end]) {
            Some(i) if i > base => cut = Some(begin),
            Some(_) => break,
            None => {}
        }
        if begin == 0 {
            break;
        }
        end = begin - 1;
    }
    cut.map_or("", |c| &raw[c..])
}

/// The latest served-model receipt anywhere in a raw transcript.
fn last_served_model(raw: &str) -> Option<String> {
    raw.lines().rev().find_map(|line| {
        let v = serde_json::from_str::<serde_json::Value>(line.trim()).ok()?;
        model_selection_label(v.get("content")?.as_str()?)
    })
}

pub fn read_full(conv_dir: &Path) -> Result<Transcript> {
    read_full_since(conv_dir, None)
}

/// [`read_full`] over only the steps past `since` (see [`steps_after`]) — what a
/// resumed run did, not the conversation it continued. The served model is the
/// one exception: a bare resume writes no new settings receipt, and the model the
/// conversation is on is still the latest receipt, wherever it sits.
pub fn read_full_since(conv_dir: &Path, since: Option<i64>) -> Result<Transcript> {
    let transcript = transcript_path(conv_dir);
    if !transcript.exists() {
        return Err(anyhow!(
            "transcript file not found: {}",
            transcript.display()
        ));
    }
    let raw = std::fs::read_to_string(&transcript)
        .with_context(|| format!("reading {}", transcript.display()))?;
    let own = steps_after(&raw, since);
    let mut t = parse_full(own);
    if since.is_some() && t.served_model.is_none() {
        t.served_model = last_served_model(&raw);
    }
    if !t.pending_tasks.is_empty() {
        let mut still_pending = Vec::new();
        for id in t.pending_tasks.drain(..) {
            match read_task_log_result(conv_dir, &id) {
                Some(r) => t.task_results.push(r),
                None => still_pending.push(id),
            }
        }
        t.pending_tasks = still_pending;
    }
    if !t.task_results.is_empty() {
        let cmds = launch_commands(own);
        for r in &mut t.task_results {
            if r.cmd.is_none() {
                r.cmd = cmds.get(&r.id).cloned();
            }
        }
    }
    if t.answer.is_empty() {
        // A logged ERROR_MESSAGE receipt names the real cause — surface that
        // instead of guessing trust/OAuth. 429 specifically is the model
        // being rate-limited/overloaded, which on Antigravity's subscription
        // tiers is how a quota wall actually shows up headlessly (the IDE's
        // "Baseline model quota reached" banner has no headless equivalent).
        let msg = if !t.errors.is_empty() {
            let is_429 = t.errors.iter().any(|e| crate::agy::quota::is_quota_wall(e));
            let hint = if is_429 {
                "this is a rate-limit/quota condition (HTTP 429), NOT a trust/OAuth issue — \
                 the model was overloaded or your subscription quota for it is exhausted. \
                 Wait and retry, switch model (e.g. flash), or check the Antigravity app for \
                 quota reset time."
            } else {
                "this is an API error agy itself logged, NOT a trust/OAuth issue."
            };
            format!(
                "agy logged an error and produced no answer ({}): {} — {}",
                transcript.display(),
                t.errors.join("; "),
                hint
            )
        } else if since.is_some() && own.trim().is_empty() {
            // Not the trust prompt: the conversation is trusted and has turns. The
            // resume's message was never turned into a step, so there is nothing of
            // this run's to report — and the previous turn's outcome is not this one.
            format!(
                "agy added no steps to the resumed conversation ({}); the new message was \
                 never processed, so this run has no outcome of its own. The previous turn's \
                 steps are not this run's result.",
                transcript.display()
            )
        } else if t.step_summary.is_empty() && t.artifacts.is_empty() {
            // Only a transcript with nothing in it is evidence of the trust prompt.
            format!(
                "agy produced an empty/contentless transcript ({}). Most often this means agy is \
                 waiting on a first-run \"trust this folder?\" prompt that headless mode can't \
                 answer — open agy interactively once in this workspace to trust it, or use a \
                 workspace you've already trusted. (Other cause: an expired OAuth token — refresh \
                 by running agy once interactively.)",
                transcript.display()
            )
        } else {
            // 2026-09-11: 48 completed `run_command` steps were reported with the
            // trust-prompt text above. A run that did work was cut, not never started.
            // Say plainly when the run also produced no write receipts (artifacts is empty).
            let phrase = if t.artifacts.is_empty() {
                "ended the turn with no answer and no file writes"
            } else {
                "ended the turn without writing a final answer"
            };
            format!(
                "agy {phrase} ({}); the steps it took are \
                 on disk — agy_diag(trace) shows them.",
                transcript.display()
            )
        };
        return Err(anyhow::Error::new(NoAnswerError { partial: t, msg }));
    }
    Ok(t)
}

/// Token-frugal trace of a conversation for on-demand inspection: one line per
/// step (tool calls + order + final report). `full` adds model thinking and
/// longer snippets. agy is NOT launched — this only reads the transcript.
pub fn read_trace(conv_dir: &Path, full: bool) -> Result<String> {
    let transcript = transcript_path(conv_dir);
    let raw = std::fs::read_to_string(&transcript)
        .with_context(|| format!("reading {}", transcript.display()))?;
    let line_cap = if full { 320 } else { 120 };
    let max_steps = if full { 250 } else { 70 };
    let mut out: Vec<String> = Vec::new();
    let mut n: usize = 0;
    let mut shown: usize = 0;
    let mut truncated = false;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let idx = v
            .get("step_index")
            .and_then(|x| x.as_i64())
            .unwrap_or(n as i64);
        n += 1;
        let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("?");
        let content = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
        let thinking = v.get("thinking").and_then(|x| x.as_str()).unwrap_or("");

        let mut main = if let Some(tcs) = v.get("tool_calls").and_then(|x| x.as_array()) {
            let mut names = Vec::new();
            for tc in tcs {
                let nm = tc.get("name").and_then(|x| x.as_str()).unwrap_or("?");
                let hint = [
                    "CommandLine",
                    "TargetFile",
                    "DirectoryPath",
                    "ImageName",
                    "Prompt",
                ]
                .iter()
                .find_map(|k| tc.get("args").and_then(|a| a.get(*k)).and_then(decode_arg))
                .map(|s| clip(&s, 50))
                .unwrap_or_default();
                if hint.is_empty() {
                    names.push(nm.to_string());
                } else {
                    names.push(format!("{nm}({hint})"));
                }
            }
            format!("call {}", names.join(", "))
        } else if !content.is_empty() {
            clip(&strip_boiler(content), line_cap)
        } else {
            String::new()
        };
        if main.is_empty() {
            if full && !thinking.is_empty() {
                main = format!("[think] {}", clip(thinking, line_cap));
            } else {
                continue; // skip empty noise (e.g. CONVERSATION_HISTORY)
            }
        }
        if shown >= max_steps {
            truncated = true;
            break;
        }
        out.push(format!("{idx:>2} [{typ}] {main}"));
        shown += 1;
        if full && !thinking.is_empty() && !main.starts_with("[think]") {
            out.push(format!("    ↳ think: {}", clip(thinking, line_cap)));
        }
    }
    if out.is_empty() {
        return Err(anyhow!("no readable steps in {}", transcript.display()));
    }
    if truncated {
        out.push("…(truncated; pass full=true / inspect on disk for more)".to_string());
    }
    Ok(out.join("\n"))
}

fn fmt_age(d: Duration) -> String {
    let s = d.as_secs();
    if s < 90 {
        format!("{s}s")
    } else if s < 5400 {
        format!("{}m", s / 60)
    } else if s < 172_800 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

pub fn fmt_tok(t: u64) -> String {
    if t >= 1000 {
        format!("{:.1}k", t as f64 / 1000.0)
    } else {
        format!("{t}")
    }
}

/// Rough context-size estimate (tokens ≈ transcript bytes / 4) for a session.
/// Used by the report footer and the Flash-continue guard.
pub fn session_ctx_tokens(conv_dir: &Path) -> u64 {
    std::fs::metadata(transcript_path(conv_dir))
        .map(|m| m.len() / 4)
        .unwrap_or(0)
}

/// The model a session is bound to (from the transcript receipt), if readable.
/// Lets the caller refuse to continue a heavy session on a weak model. Cheap
/// targeted scan for the receipt line — avoids a full transcript parse just for
/// the Flash-continue guard (slot_of only needs the family name).
pub fn session_model(conv_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(transcript_path(conv_dir)).ok()?;
    let mut last = None;
    for line in raw.lines() {
        let Some(idx) = line.find("`Model Selection` from ") else {
            continue;
        };
        let rest = &line[idx..];
        let Some(to) = rest.find(" to ") else {
            continue;
        };
        let after = &rest[to + 4..];
        // End at sentence break ". " — labels like "Gemini 3.5 Flash (High)"
        // contain a version dot, so we can't stop at the first '.'.
        let end = after.find(". ").unwrap_or(after.len());
        let label = after[..end]
            .trim_end_matches('.')
            .trim_end_matches('\\')
            .trim();
        if crate::agy::models::is_plausible_model_label(label) {
            last = Some(label.to_string()); // keep the latest = current model
        }
    }
    last
}

/// Strip the `<USER_REQUEST>` wrapper and drop the `<ADDITIONAL_METADATA>` tail.
fn clean_user_prompt(c: &str) -> String {
    let mut s = c.to_string();
    if let Some(i) = s.find("<ADDITIONAL_METADATA>") {
        s.truncate(i);
    }
    s = s
        .replace("<USER_REQUEST>", " ")
        .replace("</USER_REQUEST>", " ");
    // Every agy_sub prompt opens with the same workspace directive, so a session list
    // showed one identical head per row. Show the caller's own task instead.
    if s.trim_start().starts_with(WORKSPACE_DIRECTIVE_HEAD) {
        if let Some(i) = s.find(WORKSPACE_DIRECTIVE_END) {
            s.drain(..i + WORKSPACE_DIRECTIVE_END.len());
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How `prepend_workspace_directive` (main.rs) opens and closes its block.
pub(crate) const WORKSPACE_DIRECTIVE_HEAD: &str = "Work inside the project directory: ";
pub(crate) const WORKSPACE_DIRECTIVE_END: &str = "\n\n---\n";

/// (step count, opening user prompt) for a transcript file.
fn session_brief(tp: &Path) -> (usize, String) {
    let Ok(raw) = std::fs::read_to_string(tp) else {
        return (0, String::new());
    };
    let mut steps = 0usize;
    let mut first = String::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        steps += 1; // transcripts are JSONL; one non-empty line ≈ one step
                    // Only parse JSON until the opening prompt is found — avoids parsing every
                    // line of a large transcript just to build the session-list header.
        if first.is_empty() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                let src = v.get("source").and_then(|x| x.as_str()).unwrap_or("");
                let typ = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                if typ == "USER_INPUT" || src.starts_with("USER") {
                    if let Some(c) = v.get("content").and_then(|x| x.as_str()) {
                        first = clip(&clean_user_prompt(c), 60);
                    }
                }
            }
        }
    }
    (steps, first)
}

/// List recent conversations (newest first) with a rough context-size estimate,
/// so the caller can weigh resume-reuse vs context bloat. Spends no quota.
pub fn list_sessions(limit: usize) -> String {
    let mut items: Vec<(SystemTime, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(brain_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let tp = transcript_path(&p);
            let mt = std::fs::metadata(&tp)
                .and_then(|m| m.modified())
                .or_else(|_| e.metadata().and_then(|m| m.modified()))
                .ok();
            if let Some(mt) = mt {
                items.push((mt, p));
            }
        }
    }
    items.sort_by(|a, b| b.0.cmp(&a.0));
    let limit = limit.max(1);
    let mut rows: Vec<String> = Vec::new();
    for (mt, dir) in &items {
        if rows.len() >= limit {
            break;
        }
        let tp = transcript_path(dir);
        let (steps, first) = session_brief(&tp);
        // Skip empty sessions (e.g. ConPTY usage/model scrapes leave a bare dir).
        if steps == 0 && first.is_empty() {
            continue;
        }
        let id = dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let short = id.split('-').next().unwrap_or(&id);
        let age = mt.elapsed().ok().map(fmt_age).unwrap_or_else(|| "?".into());
        let bytes = std::fs::metadata(&tp).map(|m| m.len()).unwrap_or(0);
        rows.push(format!(
            "  {short}  {age:>5}  {steps:>3} steps  ~{:>5} tok  \"{first}\"",
            fmt_tok(bytes / 4)
        ));
    }
    if rows.is_empty() {
        return "no agy conversations found.".to_string();
    }
    let mut lines =
        vec!["recent agy sessions (newest first; ~tok ≈ transcript bytes/4, rough):".to_string()];
    lines.extend(rows);
    lines.push(
        "resume: agy_sub(conversation_id=<id>) · inspect: agy_diag(trace, conversation_id=<id>)"
            .to_string(),
    );
    lines.join("\n")
}

pub fn parse_last_tool_call(transcript_raw: &str) -> Option<(String, String)> {
    let mut last_call = None;
    for line in transcript_raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(tcs) = v.get("tool_calls").and_then(|x| x.as_array()) else {
            continue;
        };
        if tcs.is_empty() {
            continue;
        }
        if let Some(first) = tcs.first() {
            let name = first.get("name").and_then(|x| x.as_str()).unwrap_or("");
            // `Url` is here for `read_url_content`: agy 1.1.28 made URL fetching
            // ask-first, and the denial message names only a `<target>` placeholder,
            // so the transcript is the ONLY place the actual URL appears. Without
            // this key the grant rule gets an empty target and refuses to recover.
            let target = ["CommandLine", "TargetFile", "DirectoryPath", "Url"]
                .iter()
                .find_map(|k| {
                    first
                        .get("args")
                        .and_then(|a| a.get(*k))
                        .and_then(decode_arg)
                })
                .unwrap_or_default();
            last_call = Some((name.to_string(), target));
        }
    }
    last_call
}

pub fn read_last_tool_call(conv_dir: &Path) -> Option<(String, String)> {
    let path = transcript_path(conv_dir);
    let raw = std::fs::read_to_string(path).ok()?;
    parse_last_tool_call(&raw)
}

/// Every `run_command` line's command text, in order, from a raw transcript
/// (JSONL). Independent of `Transcript`/`parse_full` — like
/// `recent_step_timestamps` above — so a footer check that only needs the
/// command lines doesn't need a new `Transcript` field threaded through every
/// existing call site and test literal. Feeds `repeated_command_warning` (this
/// run's own step trace) and the stray-artifact check (does a stray file's
/// path appear in something this run actually ran).
pub(crate) fn run_command_lines(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(tcs) = v.get("tool_calls").and_then(|x| x.as_array()) else {
            continue;
        };
        for tc in tcs {
            if tc.get("name").and_then(|x| x.as_str()) != Some("run_command") {
                continue;
            }
            if let Some(cmd) = tc
                .get("args")
                .and_then(|a| a.get("CommandLine"))
                .and_then(decode_arg)
            {
                out.push(cmd);
            }
        }
    }
    out
}

/// [`run_command_lines`], reading the conversation's transcript off disk.
/// Empty when unreadable — a missing/unreadable transcript just means
/// "nothing to warn about", the same posture as the other on-disk-only footer
/// reads (e.g. `read_last_tool_call`).
pub fn run_command_lines_for(conv_dir: &Path) -> Vec<String> {
    let path = transcript_path(conv_dir);
    std::fs::read_to_string(path)
        .map(|raw| run_command_lines(&raw))
        .unwrap_or_default()
}

/// The identifying argument for a tool call's STATUS-LINE display, or empty when
/// the tool has none worth showing. Separate from `parse_last_tool_call`'s target
/// (which exists to re-issue the exact same call for an auto-grant, so it wants
/// the full path/URL/command) — this one is read by a human polling a running
/// job, so `run_command` still shows the whole command line (that IS the action),
/// but a file tool shows only the file NAME, not its full path, to keep the line
/// short and to avoid repeating the workspace root on every poll.
fn step_detail(name: &str, args: Option<&serde_json::Value>) -> String {
    let Some(args) = args else {
        return String::new();
    };
    if name == "run_command" {
        return args
            .get("CommandLine")
            .and_then(decode_arg)
            .unwrap_or_default();
    }
    // Edit tools name the file `TargetFile`; view_file names it `AbsolutePath`
    // (agy 1.2.x transcripts). Reading only the first left the most frequent step
    // of a run — reading files — as a bare "view_file".
    ["TargetFile", "AbsolutePath"]
        .iter()
        .find_map(|k| args.get(*k).and_then(decode_arg))
        .map(|f| {
            Path::new(&f)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or(f)
        })
        .unwrap_or_default()
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date (UTC, no leap
/// seconds) — Howard Hinnant's `days_from_civil` algorithm
/// (howardhinnant.github.io/date_algorithms.html). Hand-rolled because this repo
/// takes no new dependencies and nowhere else needs calendar math (everything
/// else is unix-second arithmetic on `SystemTime`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11]: Mar=0 .. Feb=11
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Parse a UTC transcript timestamp ("2026-09-14T20:21:55Z") into unix seconds.
/// No fractional-second or offset support — agy's own `created_at` doesn't carry
/// them — so an unrecognized shape returns None instead of guessing an age.
fn parse_created_at_unix(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let time = time.split('.').next().unwrap_or(time); // drop any fractional seconds
    let mut d = date.splitn(3, '-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: u32 = d.next()?.parse().ok()?;
    let da: u32 = d.next()?.parse().ok()?;
    let mut t = time.splitn(3, ':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let se: i64 = t.next()?.parse().ok()?;
    let secs = days_from_civil(y, mo, da) * 86400 + h * 3600 + mi * 60 + se;
    u64::try_from(secs).ok()
}

/// Median interval (seconds) between the timestamps of the last `n` transcript
/// steps (tool calls), for the poll running line's `~Ns/step`.
///
/// A single "(Ns ago)" age can't tell a run that steps steadily every 90s from
/// one that was fast until it suddenly went quiet — both show the same age on
/// the next poll. The pace fills that gap. Needs at least 3 timestamps (2
/// intervals) to say anything; fewer returns None rather than a guess from one
/// data point. Pure/testable independent of `last_step_line`'s formatting.
pub(crate) fn median_step_interval_s(timestamps: &[u64]) -> Option<u64> {
    if timestamps.len() < 3 {
        return None;
    }
    let mut sorted = timestamps.to_vec();
    sorted.sort_unstable();
    let mut intervals: Vec<u64> = sorted
        .windows(2)
        .map(|w| w[1].saturating_sub(w[0]))
        .collect();
    intervals.sort_unstable();
    let mid = intervals.len() / 2;
    Some(if intervals.len().is_multiple_of(2) {
        (intervals[mid - 1] + intervals[mid]).div_ceil(2) // round half up
    } else {
        intervals[mid]
    })
}

/// Timestamps (unix seconds) of the last `n` transcript entries carrying a tool
/// call, oldest first. Feeds `median_step_interval_s`. An entry with no
/// parseable `created_at` is skipped rather than counted as a zero-length gap,
/// which would drag the median toward zero for no real reason.
pub(crate) fn recent_step_timestamps(raw: &str, n: usize) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let has_tool_call = v
            .get("tool_calls")
            .and_then(|x| x.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if !has_tool_call {
            continue;
        }
        let Some(t) = v
            .get("created_at")
            .and_then(|x| x.as_str())
            .and_then(parse_created_at_unix)
        else {
            continue;
        };
        out.push(t);
        if out.len() > n {
            out.remove(0);
        }
    }
    out
}

/// One line describing the most recent tool call in a transcript, for a running
/// job's status line — e.g. "run_command: cargo test -p garment (95s ago) ·
/// ~40s/step" or "view_file: transcript.rs (3s ago)". `now_unix` is a parameter
/// (not read from the wall clock) so the age math stays pure and testable.
pub(crate) fn last_step_line(raw: &str, now_unix: u64) -> Option<String> {
    let mut found: Option<(String, String, Option<u64>)> = None; // (name, detail, created_at)
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(tcs) = v.get("tool_calls").and_then(|x| x.as_array()) else {
            continue;
        };
        let Some(first) = tcs.first() else {
            continue;
        };
        let Some(name) = first.get("name").and_then(|x| x.as_str()) else {
            continue;
        };
        let detail = step_detail(name, first.get("args"));
        let created_at = v
            .get("created_at")
            .and_then(|x| x.as_str())
            .and_then(parse_created_at_unix);
        found = Some((name.to_string(), detail, created_at));
    }
    let (name, detail, created_at) = found?;
    let head = if detail.is_empty() {
        name
    } else {
        format!("{name}: {detail}")
    };
    let head = clip(&head, 80);
    let mut line = match created_at {
        Some(t) => format!("{head} ({}s ago)", now_unix.saturating_sub(t)),
        None => head,
    };
    if let Some(pace) = median_step_interval_s(&recent_step_timestamps(raw, 5)) {
        line.push_str(&format!(" · ~{pace}s/step"));
    }
    Some(line)
}

/// [`last_step_line`], reading the conversation's transcript off disk. None when
/// the transcript is missing/unreadable or has no tool call yet.
pub fn read_last_step(conv_dir: &Path, now_unix: u64) -> Option<String> {
    let path = transcript_path(conv_dir);
    let raw = std::fs::read_to_string(path).ok()?;
    last_step_line(&raw, now_unix)
}

/// Strip known agy system blocks (see SYSTEM_BLOCK_TAGS) from stdout content.
pub(crate) fn strip_system_blocks(input: &str) -> String {
    let mut result = String::new();
    let mut current = input;
    while let Some(start_idx) = current.find('<') {
        result.push_str(&current[..start_idx]);
        let rest = &current[start_idx..];

        if let Some(tag_end_idx) = rest.find('>') {
            let tag_content = &rest[1..tag_end_idx];
            if crate::agy::run::SYSTEM_BLOCK_TAGS.contains(&tag_content) {
                let close_tag = format!("</{}>", tag_content);
                if let Some(close_idx) = rest.find(&close_tag) {
                    current = &rest[close_idx + close_tag.len()..];
                    continue;
                }
            }
        }

        result.push('<');
        current = &rest[1..];
    }
    result.push_str(current);
    result
}

/// A line this many U+FFFD into it is a broken decode, not prose. One or two can
/// legitimately appear (a model quoting a replacement char); a mangled console line
/// is dense with them.
const BROKEN_LINE_MIN_REPLACEMENTS: usize = 3;

/// Drop console output agy destroyed on the way in, and say so.
///
/// agy reads child-process output as UTF-8 on a machine whose console is not
/// (CP932 here), so every non-ASCII lead byte becomes U+FFFD and only stray ASCII
/// trail bytes survive: `用語 'cargo' は…` arrives as `\u{FFFD}p\u{FFFD} 'cargo' …`.
/// **The original bytes are gone by then** — nothing downstream can restore them, so
/// the only choice is whether to pass the wreckage on. We don't: a model handed
/// mangled text recognizes the *shape* of a familiar error and rewrites it cleanly
/// from memory, producing a confident answer with no evidence under it (observed
/// 2026-07-27). An explicit marker cannot be laundered that way. Forensics:
/// design-notes "Mojibake on Windows".
pub(crate) fn strip_broken_encoding(input: &str) -> String {
    if !input.contains('\u{FFFD}') {
        return input.to_string();
    }
    let marker = |n: usize| format!("⟨{n} line(s) dropped: non-UTF-8 console output⟩");
    let mut out: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    for line in input.lines() {
        if line.chars().filter(|c| *c == '\u{FFFD}').count() >= BROKEN_LINE_MIN_REPLACEMENTS {
            dropped += 1;
            continue;
        }
        if dropped > 0 {
            out.push(marker(dropped));
            dropped = 0;
        }
        out.push(line.to_string());
    }
    if dropped > 0 {
        out.push(marker(dropped));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Transcript shape captured from a real backgrounded `cargo build` (conv
    // 393d1d64 records 18-21): launch → "waiting" planner turn → completion
    // SYSTEM_MESSAGE → EMPTY re-ignited planner turn.
    const BG_BUILD_TRANSCRIPT: &str = concat!(
        r#"{"step_index":18,"source":"MODEL","type":"RUN_COMMAND","status":"DONE","content":"Tool is running as a background task with task id: conv1/task-18 Task Description: cargo build"}"#,
        "\n",
        r#"{"step_index":19,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"ビルド実行中です。お待ちください。"}"#,
        "\n",
        r#"{"step_index":20,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","content":"[Message] sender=conv1/task-18 priority=HIGH content=Task id finished with result: The command completed successfully. Output: Finished dev profile in 92s"}"#,
        "\n",
        r#"{"step_index":21,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE"}"#,
        "\n",
    );

    #[test]
    fn captures_backgrounded_build_completion() {
        let t = parse_full(BG_BUILD_TRANSCRIPT);
        // The terminal answer is still the "waiting" turn (the re-ignited turn is empty).
        assert!(t.answer.contains("ビルド実行中"));
        // The completion is matched, nothing left pending.
        assert!(
            t.pending_tasks.is_empty(),
            "task was finished, so not pending"
        );
        assert_eq!(t.task_results.len(), 1);
        let tr = &t.task_results[0];
        assert_eq!(tr.id, "conv1/task-18");
        assert!(tr.ok, "‘completed successfully’ → ok");
        assert_eq!(tr.exit_code, Some(0));
        assert!(tr.tail.contains("Finished dev profile"));
        // The folded answer surfaces the build outcome.
        let folded = crate::agy::run::append_task_results(t.answer.clone(), &t.task_results);
        assert!(folded.contains("✓ task-18: ok (exit=0)"));
    }

    // agy's OTHER wording for the same happy path. Verbatim from the receipt of
    // job j15 / task-86 (2026-08-14), which reported `✗ task-86: FAILED` while
    // quoting `112 passed; 0 failed` and `exited with code 0` on the same line.
    // Neither `parse_exit_code`'s key list nor the `completed successfully` phrase
    // matched it, so a green test run was folded in as a failure.
    // Both wordings agy uses for an edit, including the one that puts a diff block
    // right after the path. That second shape put a whole diff plus its trailing
    // notice into `wrote:` (observed 2026-08-15) because the sentence marker it was
    // split on is simply absent there.
    #[test]
    fn an_edit_receipt_yields_a_path_and_never_the_diff_after_it() {
        let with_diff = "G:\\workshopG\\gemini-subagent\\REFERENCE.md:\n[diff_block_start]\n \
             CLI の `wait`・`poll`）はいずれもディスクの `seen` を立てます。\n[diff_block_end]\n\
             Please note that the above snippet only shows the MODIFIED lines.";
        assert_eq!(
            edit_receipt_path(with_diff).as_deref(),
            Some("G:\\workshopG\\gemini-subagent\\REFERENCE.md")
        );
        let with_sentence = "src/main.rs. If relevant, review the change and continue.";
        assert_eq!(
            edit_receipt_path(with_sentence).as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(edit_receipt_path("   \n[diff_block_start]"), None);
    }

    #[test]
    fn a_receipt_that_says_exited_with_code_0_is_not_a_failure() {
        let body = "\n\t\t\t\tThe command exited with code 0.\n\t\t\t\tOutput:\n\t\t\t\t<truncated 1 lines>\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 8.65s\ntest result: ok. 112 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.67s\n";
        assert_eq!(
            parse_exit_code(body),
            Some(0),
            "the wording agy actually used must parse"
        );
        let summary = find_test_summary(body);
        assert!(bg_finish_ok(
            body,
            parse_exit_code(body),
            summary.as_deref()
        ));
    }

    #[test]
    fn a_nonzero_exit_and_a_failing_test_count_both_outrank_the_phrase() {
        let failed = "The command exited with code 101.\ntest result: FAILED. 3 passed; 2 failed; 0 ignored\n";
        assert!(!bg_finish_ok(
            failed,
            parse_exit_code(failed),
            find_test_summary(failed).as_deref()
        ));
        // Exit 0 but the run reported failing tests: trust the count, which is the
        // side that names a defect.
        let disagreeing =
            "The command exited with code 0.\ntest result: FAILED. 3 passed; 2 failed; 0 ignored\n";
        assert!(!bg_finish_ok(
            disagreeing,
            parse_exit_code(disagreeing),
            find_test_summary(disagreeing).as_deref()
        ));
    }

    #[test]
    fn launch_commands_pairs_each_launch_with_the_command_before_it() {
        let raw = [
            r#"{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"\"cargo test\""}}]}"#,
            r#"{"type":"RUN_COMMAND","content":"running as a background task with task id: c/task-1 x"}"#,
            r#"{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"\"cargo build\""}}]}"#,
            r#"{"type":"RUN_COMMAND","content":"running as a background task with task id: c/task-2 x"}"#,
        ]
        .join("
");
        let m = launch_commands(&raw);
        assert_eq!(m.get("c/task-1").map(String::as_str), Some("cargo test"));
        assert_eq!(m.get("c/task-2").map(String::as_str), Some("cargo build"));
    }

    #[test]
    fn unfinished_background_task_stays_pending() {
        // Only the launch, no completion SYSTEM_MESSAGE yet.
        let raw = r#"{"step_index":18,"source":"MODEL","type":"RUN_COMMAND","status":"DONE","content":"running as a background task with task id: conv1/task-7 building"}"#;
        let t = parse_full(raw);
        assert_eq!(t.pending_tasks, vec!["conv1/task-7".to_string()]);
        assert!(t.task_results.is_empty());
    }

    // Empirically (2026-06-25 smoke) agy can finish a backgrounded `cargo build`
    // cleanly on disk yet never write the completion SYSTEM_MESSAGE at all — the
    // task's own log file is the only ground truth. read_full must recover via
    // read_task_log_result instead of waiting forever for a record that never lands.
    #[test]
    fn read_full_recovers_from_task_log_when_no_system_message() {
        let dir = std::env::temp_dir().join(format!("agy_test_tasklog_{}", std::process::id()));
        let tasks_dir = dir.join(".system_generated").join("tasks");
        let logs_dir = dir.join(".system_generated").join("logs");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(&logs_dir).unwrap();
        let conv_name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let raw = format!(
            "{{\"step_index\":1,\"source\":\"MODEL\",\"type\":\"RUN_COMMAND\",\"status\":\"DONE\",\"content\":\"running as a background task with task id: {conv_name}/task-48 cargo build\"}}\n\
             {{\"step_index\":2,\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"content\":\"ビルド実行中です。\"}}\n"
        );
        std::fs::write(logs_dir.join("transcript.jsonl"), &raw).unwrap();
        std::fs::write(
            tasks_dir.join("task-48.log"),
            "   Compiling reqwest v0.12.28\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 21.62s\n",
        )
        .unwrap();

        let t = read_full(&dir).unwrap_or_else(|e| panic!("read_full failed: {e:#}"));
        assert!(
            t.pending_tasks.is_empty(),
            "task log had a terminal marker — should resolve, not stay pending"
        );
        assert_eq!(t.task_results.len(), 1);
        assert!(t.task_results[0].ok);
        assert_eq!(t.task_results[0].exit_code, Some(0));
        assert!(t.task_results[0].tail.contains("Finished"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_transcript_without_an_answer_keeps_its_steps_and_names_the_right_cause() {
        let dir = std::env::temp_dir().join(format!("agy_test_noanswer_{}", std::process::id()));
        let logs = dir.join(".system_generated").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        // The 1.2.x shape of a run cut mid-turn: tool calls on content-less planner
        // rows, results in GENERIC rows, and no terminal answer.
        let worked = concat!(
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"run_command","args":{}}]}"#,
            "\n",
            r#"{"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","content":"Created At: x"}"#,
            "\n"
        );
        std::fs::write(logs.join("transcript.jsonl"), worked).unwrap();
        let err = read_full(&dir).err().expect("no answer is an error");
        let n = err
            .downcast_ref::<NoAnswerError>()
            .expect("the partial transcript must survive");
        assert_eq!(n.partial.step_summary, "1 step: 1×run_command");
        assert!(
            n.msg
                .contains("ended the turn with no answer and no file writes"),
            "{}",
            n.msg
        );
        assert!(
            !n.msg.contains("trust"),
            "a run that did work was not stuck on trust: {}",
            n.msg
        );

        // When write receipts ARE present, say "without writing a final answer".
        let worked_with_writes = concat!(
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","tool_calls":[{"name":"replace_file_content","args":{}}]}"#,
            "\n",
            r#"{"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","content":"The following changes were made by the replace_file_content tool to: C:\\proj\\src\\lib.rs. If relevant, continue."}"#,
            "\n"
        );
        std::fs::write(logs.join("transcript.jsonl"), worked_with_writes).unwrap();
        let err2 = read_full(&dir).err().expect("no answer is an error");
        let n2 = err2
            .downcast_ref::<NoAnswerError>()
            .expect("the partial transcript must survive");
        assert_eq!(
            n2.partial.artifacts,
            vec!["C:\\proj\\src\\lib.rs".to_string()]
        );
        assert!(
            n2.msg.contains("without writing a final answer"),
            "{}",
            n2.msg
        );

        // Nothing but the user's message: that one IS the trust-prompt signature.
        std::fs::write(
            logs.join("transcript.jsonl"),
            concat!(
                r#"{"step_index":0,"source":"USER","type":"USER_INPUT","status":"DONE","content":"hi"}"#,
                "\n"
            ),
        )
        .unwrap();
        let err = read_full(&dir).err().expect("no answer is an error");
        assert!(format!("{err}").contains("trust this folder"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generic_receipts_capture_edits_and_background_launches() {
        // agy 1.2.x shapes (observed in real transcripts 090b6545 and 68f84986):
        // edit receipts, file creation, and background launches are all typed GENERIC
        // and preceded by Created At: / Completed At: header lines.
        let raw = concat!(
            r#"{"step_index":1,"source":"MODEL","type":"GENERIC","status":"DONE","content":"Created At: 2026-09-15T04:38:22+09:00\nCompleted At: 2026-09-15T04:38:22+09:00\nCreated file file:///G:/workshopG/gen3dasset/crates/garment/src/remesh.rs with requested content.\nIf relevant, continue."}"#,
            "\n",
            r#"{"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","content":"Created At: 2026-09-15T04:39:07+09:00\nCompleted At: 2026-09-15T04:39:07+09:00\nThe following changes were made by the replace_file_content tool to: G:\\workshopG\\gen3dasset\\crates\\garment\\src\\lib.rs. If relevant, continue.\n[diff_block_start]\n@@ -11,7 +11,7 @@\n[diff_block_end]"}"#,
            "\n",
            r#"{"step_index":3,"source":"MODEL","type":"GENERIC","status":"RUNNING","content":"Created At: 2026-09-15T04:48:23+09:00\nTool is running as a background task with task id: 68f84986-fb8a-4f02-b918-89d3317a2d96/task-142\nTask Description: cargo test"}"#,
            "\n"
        );
        let t = parse_full(raw);
        assert_eq!(
            t.artifacts,
            vec![
                "G:\\workshopG\\gen3dasset\\crates\\garment\\src\\remesh.rs".to_string(),
                "G:\\workshopG\\gen3dasset\\crates\\garment\\src\\lib.rs".to_string(),
            ]
        );
        assert_eq!(
            t.pending_tasks,
            vec!["68f84986-fb8a-4f02-b918-89d3317a2d96/task-142".to_string()]
        );
    }

    #[test]
    fn generic_view_file_with_receipt_markers_yields_no_artifacts_or_tasks() {
        // A GENERIC view_file-shaped result whose file text contains receipt and task
        // launch phrases mid-content must yield NO artifacts and NO pending tasks.
        let raw = concat!(
            r#"{"step_index":1,"source":"MODEL","type":"GENERIC","status":"DONE","content":"Created At: 2026-09-15T18:00:00+09:00\nCompleted At: 2026-09-15T18:00:01+09:00\nFile Path: `file:///G:/workshopG/gemini-subagent/src/agy/transcript.rs`\nTotal Lines: 1400\n// Some code\nThe following changes were made by the replace_file_content tool to: C:\\x.rs\nCreated file file:///C:/y.rs\nrunning as a background task with task id: c/task-9\n"}"#,
            "\n"
        );
        let t = parse_full(raw);
        assert!(
            t.artifacts.is_empty(),
            "expected no artifacts, got: {:?}",
            t.artifacts
        );
        assert!(
            t.pending_tasks.is_empty(),
            "expected no pending tasks, got: {:?}",
            t.pending_tasks
        );
    }

    #[test]
    fn test_first_meaningful_line_skips_headers() {
        let content = "Created At: 2026-09-15T00:00:00Z\nCompleted At: 2026-09-15T00:00:01Z\nUsing prompt: test\n\nActual first line\nSecond line";
        assert_eq!(first_meaningful_line(content), Some("Actual first line"));

        let empty = "Created At: 2026-09-15T00:00:00Z\n\nCompleted At: 2026-09-15T00:00:01Z\n";
        assert_eq!(first_meaningful_line(empty), None);
    }

    #[test]
    fn artifacts_capture_edits_and_drop_brain_bookkeeping() {
        // Need absolute path for test to work on both Unix and Windows
        let brain_p = brain_dir();
        let brain_str = brain_p.to_string_lossy();
        let brain_url = if cfg!(windows) {
            format!("file:///{}", brain_str.replace('\\', "/"))
        } else {
            format!("file://{}", brain_str)
        };

        let raw = format!(
            "{{\"type\":\"CODE_ACTION\",\"content\":\"Created file {brain_url}/task.md with requested content.\"}}\n\
             {{\"type\":\"CODE_ACTION\",\"content\":\"Created file file:///C:/proj/new.rs with requested content.\"}}\n\
             {{\"type\":\"CODE_ACTION\",\"content\":\"The following changes were made by the multi_replace_file_content \
             tool to: C:\\\\proj\\\\existing.rs. If relevant, proactively run terminal commands.\"}}\n"
        );
        let t = parse_full(&raw);
        assert_eq!(
            t.artifacts,
            vec![
                "C:\\proj\\new.rs".to_string(),
                "C:\\proj\\existing.rs".to_string()
            ],
            "brain-dir bookkeeping file dropped; created + edited project files kept"
        );
    }

    #[test]
    fn artifacts_capture_agy_1_4_edit_file_and_write_file_receipts() {
        // Real shapes from agy 1.4.9 transcripts (2026-10-29): `edit_file` names the
        // path in its receipt; `write_file`'s receipt is a bare "Created file.", so
        // the path comes from the call's JSON-encoded `Path` argument. A failed
        // edit and a write whose call step was superseded record nothing.
        let raw = r#"{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"write_file","args":{"Content":"\"x\"","Path":"\"C:\\\\proj\\\\probe.txt\""}}]}
{"type":"GENERIC","content":"Created At: 2026-10-29T07:53:24+09:00\nCompleted At: 2026-10-29T07:53:24+09:00\nCreated file."}
{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"edit_file","args":{"Path":"\"C:\\\\proj\\\\src\\\\a.rs\""}}]}
{"type":"GENERIC","content":"Created At: x\nMade 2 replacements in C:\\proj\\src\\a.rs starting at line 12 (+1/-0 lines)."}
{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"edit_file","args":{}}]}
{"type":"GENERIC","content":"Encountered error in step execution: target content not found in the file"}
{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"write_file","args":{"Path":"\"C:\\\\proj\\\\lost.txt\""}}]}
{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"read_file","args":{}}]}
{"type":"GENERIC","content":"Created file."}
"#;
        let t = parse_full(raw);
        assert_eq!(
            t.artifacts,
            vec!["C:\\proj\\probe.txt".to_string(), "C:\\proj\\src\\a.rs".to_string()]
        );
    }

    #[test]
    fn test_strip_broken_encoding() {
        // Real shape, from a run at 2026-07-27 01:26: PowerShell's CP932
        // CommandNotFoundException read as UTF-8. It IS "cargo is not on PATH", but
        // no reader — human or model — can recover that from these bytes.
        let mangled = "cargo : \u{FFFD}p\u{FFFD} 'cargo' \u{FFFD}\u{FFFD}A\u{FFFD}R\u{FFFD}}\u{FFFD}h\u{FFFD}\u{FFFD}\u{FFFD}b\u{FFFD}g";
        let out = strip_broken_encoding(&format!("before\n{mangled}\nafter"));
        assert_eq!(
            out,
            "before\n⟨1 line(s) dropped: non-UTF-8 console output⟩\nafter"
        );

        // Consecutive broken lines collapse into one marker carrying the count.
        let out = strip_broken_encoding(&format!("{mangled}\n{mangled}\nok"));
        assert_eq!(out, "⟨2 line(s) dropped: non-UTF-8 console output⟩\nok");

        // Clean text is returned byte-identical — the common path must not be
        // reformatted (no re-joining, no trailing-newline surprise).
        let clean = "日本語の回答\n\nコード:\n```rust\nfn main() {}\n```\n";
        assert_eq!(strip_broken_encoding(clean), clean);

        // One or two replacement chars are not a broken decode.
        assert_eq!(strip_broken_encoding("a \u{FFFD} b"), "a \u{FFFD} b");
    }

    #[test]
    fn test_strip_system_blocks() {
        assert_eq!(strip_system_blocks("hello world"), "hello world");
        assert_eq!(
            strip_system_blocks("hello <task-notification>foo bar</task-notification> world"),
            "hello  world"
        );
        assert_eq!(
            strip_system_blocks("<USER_SETTINGS_CHANGE>abc</USER_SETTINGS_CHANGE>def"),
            "def"
        );
        // Unknown tags are NOT stripped — coding answers legitimately contain XML/HTML.
        assert_eq!(
            strip_system_blocks("<tag>foo</tag> <other-tag>bar</other-tag>"),
            "<tag>foo</tag> <other-tag>bar</other-tag>"
        );
        // Nested system blocks inside a task-notification go with it.
        assert_eq!(
            strip_system_blocks(
                "A<task-notification>x</task-notification><stdout>y</stdout><log>z</log>B"
            ),
            "AB"
        );
        assert_eq!(strip_system_blocks("hello <tag>foo"), "hello <tag>foo");
        assert_eq!(strip_system_blocks("hello </tag>foo"), "hello </tag>foo");
    }

    #[test]
    fn test_read_task_log_result_non_cargo_and_cargo() {
        let unique_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_path = std::env::temp_dir().join(format!("agy_test_bg_task_{unique_id}"));
        let tasks_dir = temp_path.join(".system_generated").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        // Jest style failure
        let jest_log = tasks_dir.join("task-jest.log");
        std::fs::write(
            &jest_log,
            "PASS src/foo.test.js\nFAIL src/bar.test.js\nTests: 6 failed, 5 passed, 11 total\n",
        )
        .unwrap();
        let res_jest = read_task_log_result(&temp_path, "task-jest").unwrap();
        assert!(!res_jest.ok);
        assert_eq!(res_jest.exit_code, Some(1));

        // Vitest style failure
        let vitest_log = tasks_dir.join("task-vitest.log");
        std::fs::write(
            &vitest_log,
            "Test Files  1 failed | 2 passed (3)\nTests  1 failed | 5 passed (6)\n",
        )
        .unwrap();
        let res_vitest = read_task_log_result(&temp_path, "task-vitest").unwrap();
        assert!(!res_vitest.ok);
        assert_eq!(res_vitest.exit_code, Some(1));

        // Fully passing npm/vitest/jest-style log
        let pass_log = tasks_dir.join("task-pass.log");
        std::fs::write(&pass_log, "Tests: 0 failed, 10 passed, 10 total\n").unwrap();
        let res_pass = read_task_log_result(&temp_path, "task-pass").unwrap();
        assert!(res_pass.ok);
        assert_eq!(res_pass.exit_code, Some(0));

        // Existing cargo markers continue to pass
        let cargo_ok = tasks_dir.join("task-cargo-ok.log");
        std::fs::write(
            &cargo_ok,
            "   Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.05s\n",
        )
        .unwrap();
        let res_cargo_ok = read_task_log_result(&temp_path, "task-cargo-ok").unwrap();
        assert!(res_cargo_ok.ok);
        assert_eq!(res_cargo_ok.exit_code, Some(0));

        let cargo_err = tasks_dir.join("task-cargo-err.log");
        std::fs::write(
            &cargo_err,
            "error[E0425]: cannot find value `foo` in this scope\n",
        )
        .unwrap();
        let res_cargo_err = read_task_log_result(&temp_path, "task-cargo-err").unwrap();
        assert!(!res_cargo_err.ok);
        assert_eq!(res_cargo_err.exit_code, Some(1));

        let _ = std::fs::remove_dir_all(&temp_path);
    }

    #[test]
    fn cargo_test_results_are_summed_across_suites() {
        // Unit + one integration file + doc-tests: the last line alone says 0.
        let out = "test result: ok. 111 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.7s\n\
                   Running tests/it.rs\n\
                   test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n\
                   Doc-tests x\n\
                   test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
        let sum = find_test_summary(out).unwrap();
        assert_eq!(
            sum,
            "test result: ok. 126 passed; 0 failed; 2 ignored (sum of 3 suites)"
        );
        assert_eq!(parse_test_summary_failed_count(&sum), Some((0, true)));

        let failing = "test result: ok. 10 passed; 0 failed; 0 ignored\n\
                       test result: FAILED. 4 passed; 2 failed; 0 ignored\n";
        let sum = find_test_summary(failing).unwrap();
        assert!(
            sum.starts_with("test result: FAILED. 14 passed; 2 failed;"),
            "{sum}"
        );
        assert_eq!(parse_test_summary_failed_count(&sum), Some((2, true)));

        // A single line is quoted unchanged.
        let one = "test result: ok. 38 passed; 0 failed; 0 ignored; 0 measured";
        assert_eq!(find_test_summary(one).as_deref(), Some(one));
    }

    #[test]
    fn test_parse_test_summary_failed_count() {
        assert_eq!(
            parse_test_summary_failed_count("test result: ok. 38 passed; 0 failed; 0 ignored"),
            Some((0, true))
        );
        assert_eq!(
            parse_test_summary_failed_count("test result: FAILED. 30 passed; 6 failed; 0 ignored"),
            Some((6, true))
        );
        assert_eq!(
            parse_test_summary_failed_count("test result: 30 passed; 6 failed; 0 ignored"),
            Some((6, true))
        );
        assert_eq!(
            parse_test_summary_failed_count("Tests: 2 failed, 10 passed, 12 total"),
            Some((2, true))
        );
        assert_eq!(
            parse_test_summary_failed_count("Test Files  1 failed | 4 passed (5)"),
            Some((1, true))
        );
        assert_eq!(
            parse_test_summary_failed_count("random log line without summary"),
            None
        );
    }

    #[test]
    fn test_read_task_log_result_finished_and_failed_tests() {
        let content = "   Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.05s\nRunning unittests\ntest result: FAILED. 30 passed; 6 failed; 0 ignored";
        let summary = find_test_summary(content);
        let res = analyze_task_log_content(content, summary);
        assert_eq!(res, Some((false, Some(1))));
    }

    /// Real transcript shape from the 2026-09-09 probe: agy 1.1.28 asks before
    /// fetching a URL, and its denial message names only a `<target>` placeholder,
    /// so this transcript line is the only place the actual URL exists. An empty
    /// target here means `auto_grant_rule` cannot recover the run.
    #[test]
    fn a_url_fetch_yields_the_url_as_its_target() {
        let raw = r#"{"tool_calls":[{"name":"read_url_content","args":{"Url":"\"https://example.com\"","toolAction":"\"Fetching website content\""}}]}"#;
        assert_eq!(
            parse_last_tool_call(raw),
            Some((
                "read_url_content".to_string(),
                "https://example.com".to_string()
            ))
        );
    }

    #[test]
    fn test_parse_last_tool_call() {
        let jsonl = r#"{"type":"PLANNER_RESPONSE","content":"thinking"}
{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"\"cargo --version\""}}]}
{"type":"PLANNER_RESPONSE","content":"final"}
"#;
        assert_eq!(
            parse_last_tool_call(jsonl),
            Some(("run_command".to_string(), "cargo --version".to_string()))
        );
    }

    #[test]
    fn run_command_lines_collects_every_call_in_order_ignoring_other_tools() {
        let jsonl = [
            r#"{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"\"cargo build\""}}]}"#,
            r#"{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"view_file","args":{"AbsolutePath":"\"src/main.rs\""}}]}"#,
            r#"{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"\"cargo test\""}}]}"#,
            r#"{"type":"PLANNER_RESPONSE","content":"no tool call here"}"#,
        ]
        .join("\n");
        assert_eq!(
            run_command_lines(&jsonl),
            vec!["cargo build".to_string(), "cargo test".to_string()]
        );
        assert!(run_command_lines("").is_empty());
        assert!(run_command_lines("not json at all").is_empty());
    }

    #[test]
    fn parses_agys_created_at_timestamp_into_unix_seconds() {
        // 2026-09-14T20:21:55Z, checked against a known reference point: exactly
        // 30 days (2,592,000s) after 2026-08-15T20:21:55Z.
        assert_eq!(
            parse_created_at_unix("2026-09-14T20:21:55Z"),
            parse_created_at_unix("2026-08-15T20:21:55Z").map(|t| t + 30 * 86400)
        );
        // The one value every implementation has to agree on.
        assert_eq!(parse_created_at_unix("1970-01-01T00:00:00Z"), Some(0));
        // No offset/fractional-second support — an unrecognized shape is None,
        // never a guessed age.
        assert_eq!(parse_created_at_unix("2026-09-14T20:21:55+09:00"), None);
        assert_eq!(parse_created_at_unix("not a timestamp"), None);
    }

    #[test]
    fn last_step_line_names_the_running_command_and_its_age() {
        let jsonl = concat!(
            r#"{"type":"PLANNER_RESPONSE","content":"thinking"}"#,
            "\n",
            r#"{"type":"PLANNER_RESPONSE","created_at":"2026-09-14T20:20:00Z","tool_calls":[{"name":"run_command","args":{"CommandLine":"\"cargo test -p garment\""}}]}"#,
            "\n",
        );
        // now = 95s after the call started.
        let now = parse_created_at_unix("2026-09-14T20:20:00Z").unwrap() + 95;
        assert_eq!(
            last_step_line(jsonl, now).as_deref(),
            Some("run_command: cargo test -p garment (95s ago)")
        );
    }

    #[test]
    fn last_step_line_shows_a_file_tools_name_not_its_full_path() {
        // The real shapes: view_file carries `AbsolutePath`, edit tools `TargetFile`.
        let jsonl = r#"{"type":"PLANNER_RESPONSE","created_at":"2026-09-14T20:20:00Z","tool_calls":[{"name":"view_file","args":{"AbsolutePath":"\"G:\\\\workshopG\\\\gemini-subagent\\\\src\\\\agy\\\\transcript.rs\""}}]}"#;
        let now = parse_created_at_unix("2026-09-14T20:20:00Z").unwrap() + 3;
        assert_eq!(
            last_step_line(jsonl, now).as_deref(),
            Some("view_file: transcript.rs (3s ago)")
        );
        let edit = r#"{"type":"PLANNER_RESPONSE","created_at":"2026-09-14T20:20:00Z","tool_calls":[{"name":"replace_file_content","args":{"TargetFile":"\"G:\\\\workshopG\\\\gemini-subagent\\\\src\\\\main.rs\""}}]}"#;
        assert_eq!(
            last_step_line(edit, now).as_deref(),
            Some("replace_file_content: main.rs (3s ago)")
        );
    }

    #[test]
    fn last_step_line_falls_back_to_the_bare_tool_name() {
        // A tool with no identifying arg (and no created_at) shows just its name,
        // never a stray ": " with nothing after it, and no bogus age.
        let jsonl = r#"{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"list_dir","args":{}}]}"#;
        assert_eq!(last_step_line(jsonl, 1_000).as_deref(), Some("list_dir"));
    }

    #[test]
    fn last_step_line_takes_the_most_recent_call_and_clips_long_ones() {
        let long_cmd = "x".repeat(200);
        let first_line = r#"{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"\"first\""}}]}"#;
        let second_line = format!(
            r#"{{"type":"PLANNER_RESPONSE","tool_calls":[{{"name":"run_command","args":{{"CommandLine":"\"{long_cmd}\""}}}}]}}"#
        );
        let jsonl = format!("{first_line}\n{second_line}\n");
        // The most recent call is the long one — not "first".
        let line = last_step_line(&jsonl, 1_000).expect("a tool call exists");
        assert!(line.starts_with("run_command: xxx"), "{line}");
        assert!(line.contains('…'), "{line}");
        assert!(line.chars().count() <= 81, "{line}"); // clip(80) + the ellipsis char
    }

    #[test]
    fn last_step_line_is_none_without_any_tool_call() {
        let jsonl = r#"{"type":"PLANNER_RESPONSE","content":"just talking"}"#;
        assert_eq!(last_step_line(jsonl, 1_000), None);
    }

    #[test]
    fn median_step_interval_needs_at_least_three_timestamps() {
        assert_eq!(median_step_interval_s(&[]), None);
        assert_eq!(median_step_interval_s(&[10]), None);
        assert_eq!(median_step_interval_s(&[10, 20]), None);
        // 3 timestamps -> 2 intervals (10, 10) -> median 10.
        assert_eq!(median_step_interval_s(&[0, 10, 20]), Some(10));
        // 4 timestamps -> 3 intervals (10, 20, 10) sorted (10,10,20) -> median 10.
        assert_eq!(median_step_interval_s(&[0, 10, 30, 40]), Some(10));
        // Unordered input is sorted before differencing.
        assert_eq!(median_step_interval_s(&[40, 0, 30, 10]), Some(10));
        // Even count of intervals averages the middle two: (10,20,30,50) ->
        // intervals (10,10,20) -> odd count 3 -> median 10. Use 5 points for an
        // even interval count: (0,10,20,40,80) -> intervals (10,10,20,40) ->
        // sorted (10,10,20,40) -> mid avg of 10 and 20 rounded up -> 15.
        assert_eq!(median_step_interval_s(&[0, 10, 20, 40, 80]), Some(15));
    }

    #[test]
    fn recent_step_timestamps_keeps_only_the_last_n_tool_calls_with_a_created_at() {
        let mk = |t: &str| {
            format!(
                r#"{{"type":"PLANNER_RESPONSE","created_at":"{t}","tool_calls":[{{"name":"run_command","args":{{}}}}]}}"#
            )
        };
        let no_call = r#"{"type":"PLANNER_RESPONSE","content":"thinking"}"#.to_string();
        let no_ts = r#"{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"list_dir","args":{}}]}"#
            .to_string();
        let jsonl = [
            mk("2026-09-14T20:20:00Z"),
            no_call,
            mk("2026-09-14T20:20:10Z"),
            no_ts,
            mk("2026-09-14T20:20:20Z"),
            mk("2026-09-14T20:20:30Z"),
        ]
        .join("\n");
        let t0 = parse_created_at_unix("2026-09-14T20:20:00Z").unwrap();
        // n=2 keeps only the two most recent.
        assert_eq!(recent_step_timestamps(&jsonl, 2), vec![t0 + 20, t0 + 30]);
        // n=5 (fewer than 5 exist) keeps all four, in order, ignoring the
        // no-call and no-timestamp lines.
        assert_eq!(
            recent_step_timestamps(&jsonl, 5),
            vec![t0, t0 + 10, t0 + 20, t0 + 30]
        );
    }

    #[test]
    fn last_step_line_appends_pace_only_with_enough_timestamped_steps() {
        let mk = |t: &str, cmd: &str| {
            format!(
                r#"{{"type":"PLANNER_RESPONSE","created_at":"{t}","tool_calls":[{{"name":"run_command","args":{{"CommandLine":"\"{cmd}\""}}}}]}}"#
            )
        };
        // Only 2 timestamped steps: no pace, unchanged from before this feature.
        let two = [
            mk("2026-09-14T20:20:00Z", "a"),
            mk("2026-09-14T20:20:10Z", "b"),
        ]
        .join("\n");
        let now2 = parse_created_at_unix("2026-09-14T20:20:10Z").unwrap();
        assert_eq!(
            last_step_line(&two, now2).as_deref(),
            Some("run_command: b (0s ago)")
        );

        // 3 timestamped steps 10s apart: pace shown.
        let three = [
            mk("2026-09-14T20:20:00Z", "a"),
            mk("2026-09-14T20:20:10Z", "b"),
            mk("2026-09-14T20:20:20Z", "c"),
        ]
        .join("\n");
        let now3 = parse_created_at_unix("2026-09-14T20:20:20Z").unwrap() + 5;
        assert_eq!(
            last_step_line(&three, now3).as_deref(),
            Some("run_command: c (5s ago) · ~10s/step")
        );
    }

    #[test]
    fn trailing_quota_retries_counts_consecutive_429_at_end() {
        // Three consecutive 429 errors at the end
        let raw_3 = concat!(
            r#"{"step_index":0,"source":"USER","type":"USER_INPUT","status":"DONE","content":"hi"}"#,
            "\n",
            r#"{"step_index":1,"source":"MODEL","type":"ERROR_MESSAGE","status":"DONE","error":"API error (attempt 1): RESOURCE_EXHAUSTED (code 429): Resource has been exhausted."}"#,
            "\n",
            r#"{"step_index":2,"source":"MODEL","type":"ERROR_MESSAGE","status":"DONE","error":"API error (attempt 2): RESOURCE_EXHAUSTED (code 429): Resource has been exhausted."}"#,
            "\n",
            r#"{"step_index":3,"source":"MODEL","type":"ERROR_MESSAGE","status":"DONE","error":"API error (attempt 3): RESOURCE_EXHAUSTED (code 429): Resource has been exhausted."}"#,
            "\n"
        );
        let t3 = parse_full(raw_3);
        assert_eq!(t3.trailing_quota_retries, 3);

        // A non-quota error at the end resets it to 0
        let raw_503 = format!(
            "{raw_3}{}\n",
            r#"{"step_index":4,"source":"MODEL","type":"ERROR_MESSAGE","status":"DONE","error":"API error (attempt 4): UNAVAILABLE (code 503)"}"#
        );
        let t503 = parse_full(&raw_503);
        assert_eq!(t503.trailing_quota_retries, 0);

        // A response at the end resets it to 0
        let raw_resp = format!(
            "{raw_3}{}\n",
            r#"{"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"recovered"}"#
        );
        let t_resp = parse_full(&raw_resp);
        assert_eq!(t_resp.trailing_quota_retries, 0);
    }

    #[test]
    fn read_conv_model_finds_served_model_in_no_answer_transcript() {
        let dir = std::env::temp_dir().join(format!("agy_test_conv_model_{}", std::process::id()));
        let logs = dir.join(".system_generated").join("logs");
        std::fs::create_dir_all(&logs).unwrap();

        let raw = concat!(
            r#"{"step_index":0,"source":"USER","type":"USER_INPUT","status":"DONE","content":"<USER_SETTINGS_CHANGE>The user changed setting `Model Selection` from None to Gemini 3.5 Flash (Medium). No need to comment on this change if the user doesn't ask about it.</USER_SETTINGS_CHANGE>"}"#,
            "\n",
            r#"{"step_index":1,"source":"MODEL","type":"ERROR_MESSAGE","status":"DONE","error":"API error (attempt 1): RESOURCE_EXHAUSTED (code 429): Resource has been exhausted."}"#,
            "\n"
        );
        std::fs::write(logs.join("transcript.jsonl"), raw).unwrap();

        let model = read_conv_model(&dir);
        assert_eq!(model.as_deref(), Some("Gemini 3.5 Flash (Medium)"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn steps_after_keeps_only_the_steps_past_the_baseline() {
        let raw = concat!(
            r#"{"step_index":0,"source":"USER","type":"USER_INPUT","status":"DONE","content":"hi"}"#,
            "\n",
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"old"}"#,
            "\n",
            r#"{"step_index":2,"source":"USER","type":"USER_INPUT","status":"DONE","content":"again"}"#,
            "\n",
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"no index"}"#,
            "\n"
        );
        assert_eq!(last_step_index(raw), Some(2));
        assert_eq!(last_step_index(""), None);
        assert_eq!(steps_after(raw, None), raw);
        // Nothing past the baseline yet: the resume has written no step.
        assert_eq!(steps_after(raw, Some(2)), "");
        assert_eq!(steps_after(raw, Some(99)), "");
        // From the first step past it, lines without an index included.
        let own = steps_after(raw, Some(1));
        assert!(own.starts_with(r#"{"step_index":2,"#), "{own}");
        assert!(own.ends_with("\"no index\"}\n"), "{own}");
        assert_eq!(parse_full(own).answer, "no index");
        // No trailing newline is the same cut.
        assert_eq!(steps_after(raw.trim_end(), Some(1)), own.trim_end());
    }

    #[test]
    fn read_full_since_scopes_the_answer_but_keeps_the_conversations_model() {
        let dir = std::env::temp_dir().join(format!("agy_test_since_{}", std::process::id()));
        let logs = dir.join(".system_generated").join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let raw = concat!(
            r#"{"step_index":0,"source":"USER","type":"USER_INPUT","status":"DONE","content":"<USER_SETTINGS_CHANGE>The user changed setting `Model Selection` from None to Gemini 3.5 Flash (Medium). No need to comment on this change if the user doesn't ask about it.</USER_SETTINGS_CHANGE>"}"#,
            "\n",
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"old answer"}"#,
            "\n",
            r#"{"step_index":2,"source":"USER","type":"USER_INPUT","status":"DONE","content":"continue"}"#,
            "\n",
            r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"new answer"}"#,
            "\n"
        );
        std::fs::write(logs.join("transcript.jsonl"), raw).unwrap();

        assert_eq!(step_baseline(&dir), Some(3));
        let t = read_full_since(&dir, Some(1)).expect("the resume answered");
        assert_eq!(t.answer, "new answer");
        assert_eq!(t.served_model.as_deref(), Some("Gemini 3.5 Flash (Medium)"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_head_skips_the_workspace_directive() {
        let raw = format!(
            "<USER_REQUEST>\n{}G:\\ws\nRun shell commands DIRECTLY.{}fix the bug\n</USER_REQUEST>",
            super::WORKSPACE_DIRECTIVE_HEAD,
            super::WORKSPACE_DIRECTIVE_END
        );
        assert_eq!(super::clean_user_prompt(&raw), "fix the bug");
        // A prompt that never had the directive (e.g. a resume) is left as is.
        assert_eq!(super::clean_user_prompt("a --- b"), "a --- b");
    }
}
