use crate::agy::models::{is_plausible_model_label, slot_of, Slot};
use crate::agy::paths::{agy_mcp_home, brain_dir, newest_conv_dir};
use crate::agy::transcript::read_transcript;
use std::path::PathBuf;
use std::time::SystemTime;

/// One quota group from the "Models & Quota" page: name + weekly/5-hour limits,
/// each kept as (percent_remaining, display_string) so the display and the
/// burn-rate log read from the same parse (no re-parsing of the formatted text).
/// f64 keeps the page's 2-decimal precision for the burn-rate diff.
pub(crate) struct QGroup {
    name: String,
    weekly: Option<(f64, String)>,
    five: Option<(f64, String)>,
    /// Seconds until each bucket refreshes, from the `Refreshes in 2h 30m` line.
    weekly_reset: Option<u64>,
    five_reset: Option<u64>,
}
fn runs_log_path() -> PathBuf {
    agy_mcp_home().join("runs.jsonl")
}
pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Leading integer of a step_summary like "3 steps: ..." (0 if none).
pub fn steps_of(step_summary: &str) -> u32 {
    step_summary
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Append one run's metadata to the run-log. Best-effort: never fails a run.
pub fn log_run(
    tool: &str,
    requested_model: Option<&str>,
    served_model: Option<&str>,
    steps: u32,
    ctx_tokens: u64,
    elapsed_s: f64,
    ok: bool,
) {
    let entry = serde_json::json!({
        "t": now_unix(),
        "tool": tool,
        "model": requested_model.unwrap_or("default"),
        "served": served_model.unwrap_or(""),
        "steps": steps,
        "ctx": ctx_tokens,
        "elapsed_s": (elapsed_s * 10.0).round() / 10.0,
        "ok": ok,
    });
    let _ = std::fs::create_dir_all(agy_mcp_home());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(runs_log_path())
    {
        use std::io::Write;
        let _ = writeln!(f, "{entry}");
    }
}

// ===========================================================================
// Run-log + stats (quota-free observation). Every agy run appends ONE compact
// JSON line to ~/.agy-mcp/runs.jsonl; agy_stats summarizes it (token-frugal
// output — never dumps raw lines).
// ===========================================================================

/// How stale a quota reading may be before an idle server refreshes it on its own.
const QUOTA_REFRESH_AFTER_S: u64 = 30 * 60;

/// Pure resolution of the quota group for a run from the requested model,
/// falling back to the conversation's own model if requested is None. Without
/// this a bare resume of a Flash conversation (no `--model` on the wire) makes
/// `record_quota_wall` learn nothing from an authoritative 429 receipt, and the
/// very next dispatch on that same conversation pays for the same wall again.
pub fn resolve_quota_group(
    requested_model: Option<&str>,
    conv_model: Option<&str>,
) -> Option<&'static str> {
    requested_model.or(conv_model).and_then(quota_group_of)
}

/// A 429 receipt IS a quota reading — record it as one.
///
/// The pre-flight guard reads `quota_log.jsonl`, which until now only ever grew
/// when someone remembered to call `agy_diag(live)`. But a run that dies on a 429
/// has just been told, authoritatively and for free, that the group is empty; the
/// TUI scrape would only confirm it. So fold the receipt straight into the log and
/// the *next* dispatch refuses up front instead of spending another round-trip
/// learning the same thing.
///
/// Returns true if a receipt was recognised and logged.
pub fn record_quota_wall(
    requested_model: Option<&str>,
    conv_model: Option<&str>,
    err_text: &str,
) -> bool {
    if !is_quota_wall(err_text) {
        return false;
    }
    let Some(group) = resolve_quota_group(requested_model, conv_model) else {
        return false;
    };
    // 0% is what the guard's "exhausted" threshold keys on. `src` marks this as
    // receipt-derived rather than scraped, so the provenance stays readable.
    let entry = serde_json::json!({
        "t": now_unix(),
        "groups": { group: { "5h": 0.0 } },
        "src": "429",
    });
    let _ = std::fs::create_dir_all(agy_mcp_home());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(quota_log_path())
    {
        use std::io::Write;
        let _ = writeln!(f, "{entry}");
    }
    true
}

/// Whether an error is a quota wall. agy 1.2.x words it `RESOURCE_EXHAUSTED (code 429)`
/// on stderr; `[429]` is the transcript receipt's form. Matching only `[429]` meant
/// `record_quota_wall` never fired once — `quota_log.jsonl` held no `src:"429"` line
/// (2026-09-14). A bare `429` is not enough: `task-429` is an ordinary task id.
pub fn is_quota_wall(err_text: &str) -> bool {
    err_text.contains("[429]")
        || err_text.contains("code 429")
        || err_text.contains("RESOURCE_EXHAUSTED")
        || agy_error_names_429(err_text)
}

/// agy's `AGY_ERROR` receipt can carry the status as `http_status=429` (flattened by
/// `parse_agy_error`) or `"code":429` (raw JSON, when it did not parse). Only counted
/// inside a failure that names `AGY_ERROR`, so a task id like `task-429` or a stray
/// `429` in output stays out; the digit after it must not continue the number.
fn agy_error_names_429(err_text: &str) -> bool {
    err_text.contains("AGY_ERROR")
        && ["=429", ":429", ": 429"].iter().any(|pat| {
            err_text
                .match_indices(pat)
                .any(|(i, _)| !err_text[i + pat.len()..].starts_with(|c: char| c.is_ascii_digit()))
        })
}

/// Whether a background quota refresh is worth doing: no reading at all, or the
/// newest one is older than [`QUOTA_REFRESH_AFTER_S`]. Pure so the staleness rule
/// is testable without touching `~/.agy-mcp`.
pub fn quota_cache_is_stale(newest_t: Option<u64>, now: u64) -> bool {
    match newest_t {
        None => true,
        Some(t) => now.saturating_sub(t) > QUOTA_REFRESH_AFTER_S,
    }
}

/// [`quota_cache_is_stale`] against the real log.
pub fn quota_needs_refresh() -> bool {
    let newest = std::fs::read_to_string(quota_log_path())
        .ok()
        .and_then(|raw| {
            raw.lines().rev().find_map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()?
                    .get("t")?
                    .as_u64()
            })
        });
    quota_cache_is_stale(newest, now_unix())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}
fn med_p90(v: &[f64], f: impl Fn(f64) -> String) -> String {
    if v.is_empty() {
        return "-".into();
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    format!(
        "med {} (p90 {})",
        f(percentile(&s, 50.0)),
        f(percentile(&s, 90.0))
    )
}
fn pct(ok: u32, n: u32) -> u32 {
    if n == 0 {
        0
    } else {
        (ok * 100 + n / 2) / n
    }
}

/// Token-frugal per-model summary of the run-log. NEVER dumps raw lines.
pub fn run_stats(limit: Option<usize>) -> String {
    let raw = match std::fs::read_to_string(runs_log_path()) {
        Ok(r) => r,
        Err(_) => {
            return "no run-log yet (~/.agy-mcp/runs.jsonl) — call agy_sub to start recording."
                .into()
        }
    };
    let mut entries: Vec<serde_json::Value> = raw
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let total = entries.len();
    let lim = limit.unwrap_or(200);
    if entries.len() > lim {
        entries = entries.split_off(entries.len() - lim);
    }

    let mut out = vec![format!(
        "agy run stats (last {} of {} runs)",
        entries.len(),
        total
    )];
    out.extend(model_stat_rows(&entries));
    let mismatches = requested_mismatch_stats(&entries);
    if !mismatches.is_empty() {
        out.push("  mismatch (requested slot ≠ served slot):".into());
        let m_width = mismatches.keys().map(|k| k.len()).max().unwrap_or(0);
        for (req, (mm, n)) in &mismatches {
            out.push(format!(
                "    {:<width$}  {}/{} ({}%)",
                req,
                mm,
                n,
                pct(*mm, *n),
                width = m_width
            ));
        }
    }
    if let Some(b) = burn_summary() {
        out.push(b);
    }
    out.join("\n")
}

/// One summary row per model, from the run-log entries. Pure, so the grouping is
/// unit-testable without a run-log on disk. Summary only — never a raw line.
pub fn model_stat_rows(entries: &[serde_json::Value]) -> Vec<String> {
    #[derive(Default)]
    struct Agg {
        disp: String,
        n: u32,
        ok: u32,
        steps: Vec<f64>,
        ctx: Vec<f64>,
        el: Vec<f64>,
    }
    let mut groups: std::collections::BTreeMap<String, Agg> = std::collections::BTreeMap::new();
    for e in entries {
        let disp = stats_group_label(
            e.get("model").and_then(|v| v.as_str()),
            e.get("served").and_then(|v| v.as_str()),
        );
        // First label seen names the row — deterministic for a given log.
        let g = groups.entry(canon_model(&disp)).or_insert_with(|| Agg {
            disp,
            ..Default::default()
        });
        g.n += 1;
        // Steps/ctx/elapsed are only meaningful for a run that finished.
        if e.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            g.ok += 1;
            if let Some(s) = e.get("steps").and_then(|v| v.as_u64()) {
                g.steps.push(s as f64);
            }
            if let Some(c) = e.get("ctx").and_then(|v| v.as_u64()) {
                g.ctx.push(c as f64);
            }
            if let Some(x) = e.get("elapsed_s").and_then(|v| v.as_f64()) {
                g.el.push(x);
            }
        }
    }

    let width = groups.values().map(|g| g.disp.len()).max().unwrap_or(0);
    groups
        .values()
        .map(|g| {
            format!(
                "  {:<width$}  n={} ok={}% · steps {} · ctx {} · {}s",
                g.disp,
                g.n,
                pct(g.ok, g.n),
                med_p90(&g.steps, |x| format!("{x:.0}")),
                med_p90(&g.ctx, |x| format!(
                    "~{}",
                    crate::agy::transcript::fmt_tok(x as u64)
                )),
                med_p90(&g.el, |x| format!("{x:.0}")),
                width = width
            )
        })
        .collect()
}

/// Which model a run-log entry counts against, as displayed in the stats rows.
///
/// **The requested model wins.** It is recorded on success and failure alike,
/// whereas `served` is a transcript receipt that a failed run never produced.
/// Keying on `served` (with the requested slug only as a fallback) therefore
/// filed a model's successes under agy's display label and its failures under
/// the requested slug — the same model as two rows, one of them 100% ok and the
/// other 0%, so no row ever showed a real ok%. That is the one number
/// tracked elsewhere. Served-model drift keeps its own report in the
/// `mismatch (requested slot ≠ served slot)` block, which is where it belongs.
///
/// Falls back to the served label when there was no explicit request (a bare
/// keep-model resume logs `"default"`), and to `"default"` when neither side
/// says anything usable.
pub fn stats_group_label(model: Option<&str>, served: Option<&str>) -> String {
    let requested = model.filter(|m| !m.is_empty() && *m != "default");
    let served = served.filter(|s| is_plausible_model_label(s));
    requested.or(served).unwrap_or("default").to_string()
}

/// Canonical grouping form: lowercase, alphanumerics only. Collapses the display
/// label ("Gemini 3.6 Flash (Low)") and the machine slug ("gemini-3.6-flash-low")
/// onto one key — both spellings reach the run-log, since `model_label` passes an
/// already-label input through unchanged. Slot-level (`slot_of`) would be too
/// coarse here: it would merge sonnet with opus, and flash-low with flash-high,
/// and comparing those ok% against each other is the point of the table.
pub fn canon_model(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

pub fn requested_mismatch_stats(
    entries: &[serde_json::Value],
) -> std::collections::BTreeMap<String, (u32, u32)> {
    let mut map = std::collections::BTreeMap::new();
    for e in entries {
        let model = e.get("model").and_then(|v| v.as_str()).unwrap_or("");
        if model.is_empty() || model == "default" {
            continue;
        }
        // A run with no served receipt says NOTHING about which model was served —
        // a failure never produced one. Counting it in the denominator makes it
        // evidence of a match and drags the rate toward zero, which is the same
        // mistake the ok% table made by keying on `served` (2026-07-27).
        let Some(served) = e
            .get("served")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let entry = map.entry(model.to_string()).or_insert((0u32, 0u32));
        entry.1 += 1;
        if crate::agy::models::slot_mismatch(model, Some(served)) {
            entry.0 += 1;
        }
    }
    map
}

pub(crate) fn quota_log_path() -> PathBuf {
    agy_mcp_home().join("quota_log.jsonl")
}

/// The accrued quota picture, from local files only — no agy launch, no TUI, no
/// quota spent. This is what `agy-mcp quota` prints.
///
/// Deliberately NOT the live scrape. That drives a full interactive agy and the
/// server serializes it against every run with the concurrency semaphore; a
/// separate CLI process has no way to take that lock, so offering `--live` here
/// would mean racing a running server. The readings this prints accrue on their
/// own anyway (`maybe_refresh_quota` after an idle run, and every 429 receipt),
/// so the cached answer is usually current — and its age is stated so a stale one
/// can't pass for fresh.
pub fn cached_quota_report() -> String {
    let Ok(raw) = std::fs::read_to_string(quota_log_path()) else {
        return "no quota readings yet (~/.agy-mcp/quota_log.jsonl). One accrues after an idle \
                run, or ask an MCP session for agy_diag(action=live)."
            .into();
    };
    let now = now_unix();
    let runs = std::fs::read_to_string(runs_log_path()).unwrap_or_default();
    let mut out = vec!["agy quota (cached — no agy launch)".to_string()];
    for group in KNOWN_GROUPS {
        let newest = raw.lines().rev().find_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            let g = v.get("groups")?.get(group)?;
            Some((
                v.get("t")?.as_u64()?,
                g.get("weekly").and_then(|x| x.as_f64()),
                g.get("5h").and_then(|x| x.as_f64()),
                v.get("src").and_then(|x| x.as_str()).unwrap_or("scrape") == "429",
                g.get("5h_reset").and_then(|x| x.as_u64()),
                g.get("weekly_reset").and_then(|x| x.as_u64()),
            ))
        });
        let Some((t, weekly, five, from_429, five_reset, weekly_reset)) = newest else {
            continue;
        };
        let refresh = match five_reset {
            Some(r) if r > now => format!(" · refreshes in {}", fmt_dur(r - now)),
            _ => String::new(),
        };
        let age = fmt_dur(now.saturating_sub(t));
        // An exhausted weekly bucket is the one that keeps a group down for days,
        // so its refresh time is worth the extra words.
        let w_refresh = match (weekly, weekly_reset) {
            (Some(x), Some(r)) if x <= QUOTA_EXHAUSTED_PCT && r > now => {
                format!(" (refreshes in {})", fmt_dur(r - now))
            }
            _ => String::new(),
        };
        let w = weekly
            .map(|x| format!("weekly {x:.1}%{w_refresh}"))
            .unwrap_or_default();
        let f = five.map(|x| format!("5h {x:.1}%")).unwrap_or_default();
        let sep = if w.is_empty() || f.is_empty() {
            ""
        } else {
            " · "
        };
        // A 0% from a 429 receipt is a fact about one moment, not a scrape of the
        // page; saying which it was stops "0%" from reading as a full reading.
        let src = if from_429 {
            " (from a 429 receipt)"
        } else {
            ""
        };
        // Same rule the guard applies, so the report can't show a 0% as current
        // while dispatch has already stopped trusting it.
        let superseded = if five.is_some()
            && effective_pct(&raw, &runs, group, Window::Five, now).is_none()
        {
            " · superseded (its refresh time passed, a later run succeeded, or a scrape missed this row)"
        } else {
            ""
        };
        out.push(format!(
            "  {group}: {w}{sep}{f}{refresh} — {age} old{src}{superseded}"
        ));
    }
    if out.len() == 1 {
        return "no quota readings for a known group yet (~/.agy-mcp/quota_log.jsonl).".into();
    }
    if let Some(b) = burn_summary() {
        out.push(b);
    }
    out.join("\n")
}

fn fmt_dur(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else {
        format!("{m}m")
    }
}

/// The groups the guard and the report know by name.
///
/// `ARGON_GROUP` is its own weekly-only bucket (agy 1.3.x, 2026-10: "For paid Pro plans,
/// Argon quota is separate"). It is OPTIONAL: a plan without the row is normal, so it is
/// never reported missing and never printed as `?`.
pub(crate) const ARGON_GROUP: &str = "GEMINI 4 ARGON";
const KNOWN_GROUPS: [&str; 3] = ["GEMINI", "CLAUDE AND GPT", ARGON_GROUP];

/// Known groups a scrape that DID parse carried no limit for. The TUI redraws in
/// frames, and a capture holding only the "CLAUDE AND GPT" row is a real outcome
/// (observed 2026-09-14, repeatedly). Silently logging only what was seen left the
/// missing group's older reading — a 429-derived 0% — authoritative for hours.
/// A group whose buckets are all `Disabled` was seen, so it is not missing.
pub(crate) fn missing_groups(groups: &[QGroup]) -> Vec<&'static str> {
    KNOWN_GROUPS
        .into_iter()
        .filter(|k| {
            *k != ARGON_GROUP
                && !groups
                    .iter()
                    .any(|g| g.name == *k && (g.weekly.is_some() || g.five.is_some()))
        })
        .collect()
}

/// Groups the usage page carries (with a limit) that the bridge has no name for. Their
/// runs fall back to the GEMINI/Claude routing in `quota_group_of`, so the guard would
/// watch the wrong bucket; saying so is the cheap half of a data-driven table.
pub(crate) fn unguarded_groups(groups: &[QGroup]) -> Vec<&str> {
    groups
        .iter()
        .filter(|g| {
            (g.weekly.is_some() || g.five.is_some()) && !KNOWN_GROUPS.contains(&g.name.as_str())
        })
        .map(|g| g.name.as_str())
        .collect()
}

/// The line `agy_diag(live)` prints for a group nobody taught the bridge.
pub(crate) fn unguarded_group_note(group: &str) -> String {
    format!(
        "⚠ quota group {group} is on the usage page but unknown to the bridge — its runs are not \
         guarded, counted or reported under it (add it to KNOWN_GROUPS and quota_group_of)"
    )
}

/// The line `agy_diag(live)` prints for a group the scrape could not see.
pub(crate) fn missing_row_note(group: &str) -> String {
    format!(
        "⚠ {group} row not found in usage screen — no fresh {group} reading; \
         its older cached one no longer blocks dispatch"
    )
}

/// Append a structured quota snapshot (from the parsed groups, NOT a re-parse of
/// the formatted display) so the burn rate accrues from ordinary agy_live(quota)
/// calls at no extra quota cost.
fn log_quota(groups: &[QGroup], missing: &[&str]) {
    let Some(entry) = quota_log_entry(groups, missing, now_unix()) else {
        return;
    };
    let _ = std::fs::create_dir_all(agy_mcp_home());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(quota_log_path())
    {
        use std::io::Write;
        let _ = writeln!(f, "{entry}");
    }
}

/// The quota-log line for one scrape, or None when it carried nothing. Pure so the
/// shape the guard reads back is pinned by a test. `missing` names known groups the
/// scrape looked for and did not find; [`parse_cached_five_pct`] treats that as
/// "the older reading for this group is no longer known to be current".
pub(crate) fn quota_log_entry(
    groups: &[QGroup],
    missing: &[&str],
    t: u64,
) -> Option<serde_json::Value> {
    let mut m = serde_json::Map::new();
    for g in groups {
        let mut o = serde_json::Map::new();
        // Resets are stored as absolute unix times, so a reader needs no scrape time.
        if let Some((w, d)) = &g.weekly {
            // Skip disabled buckets — their 0.0 sentinel isn't a real % remaining.
            if d != "disabled" {
                o.insert("weekly".into(), serde_json::json!(w));
                if let Some(r) = g.weekly_reset {
                    o.insert("weekly_reset".into(), serde_json::json!(t + r));
                }
            }
        }
        if let Some((f, d)) = &g.five {
            if d != "disabled" {
                o.insert("5h".into(), serde_json::json!(f));
                if let Some(r) = g.five_reset {
                    o.insert("5h_reset".into(), serde_json::json!(t + r));
                }
            }
        }
        if !o.is_empty() {
            m.insert(g.name.clone(), serde_json::Value::Object(o));
        }
    }
    if m.is_empty() && missing.is_empty() {
        return None;
    }
    let mut entry = serde_json::json!({ "t": t, "groups": m });
    if !missing.is_empty() {
        entry["missing"] = serde_json::json!(missing);
    }
    Some(entry)
}

/// The quota-page group a slot's usage is billed against.
pub fn group_of_slot(slot: Slot) -> &'static str {
    match slot {
        Slot::Claude => "CLAUDE AND GPT",
        Slot::Pro | Slot::Flash => "GEMINI",
    }
}

/// The quota group a model label bills against, slot or not. GPT-OSS has no
/// `model=` slot but shares the Claude bucket (the page names it), and a slot-less
/// Gemini (a lite tier, a codename) is still Gemini — so a run pinned to one through
/// `model_label` still feeds and obeys its bucket's guard.
pub fn quota_group_of(label: &str) -> Option<&'static str> {
    let l = label.to_ascii_lowercase();
    // Argon is a Gemini model with a bucket of its own; it must not fall through to
    // the shared GEMINI one (a run on it neither draws from nor is stopped by that).
    if l.contains("gemini") && l.contains("argon") {
        return Some(ARGON_GROUP);
    }
    match slot_of(label) {
        Some(s) => Some(group_of_slot(s)),
        None if l.contains("gpt-oss") => Some(group_of_slot(Slot::Claude)),
        None if l.contains("gemini") => Some(group_of_slot(Slot::Flash)),
        None => None,
    }
}

/// A reading at or below this % counts as exhausted...
pub(crate) const QUOTA_EXHAUSTED_PCT: f64 = 1.0;
/// ...and, without a logged refresh time, is trusted only this long — an older
/// "0%" may already have refilled.
pub(crate) const QUOTA_CACHE_FRESH_S: u64 = 4 * 3600;

/// The two buckets every quota group has. Which one ran out decides whether the
/// caller waits hours or days, so every refusal names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Weekly,
    Five,
}

impl Window {
    /// The quota-log key, which is also the label the caller sees.
    pub fn key(self) -> &'static str {
        match self {
            Window::Weekly => "weekly",
            Window::Five => "5h",
        }
    }
    fn reset_key(self) -> &'static str {
        match self {
            Window::Weekly => "weekly_reset",
            Window::Five => "5h_reset",
        }
    }
}

/// A cached reading the guard may act on. `reset_in_s` is the time left until the
/// bucket refreshes, when the scrape that took the reading saw it; a 429 receipt and
/// a reading logged before refresh times were kept have None. `from_receipt` marks a
/// 0% written by `record_quota_wall`, which cannot tell which window ran out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CachedPct {
    pub pct: f64,
    pub age_s: u64,
    pub reset_in_s: Option<u64>,
    pub from_receipt: bool,
}

impl CachedPct {
    /// Still says "exhausted": at/below the threshold, and before its refresh time
    /// (or, without one, recent). The parser already dropped a reading whose
    /// refresh time passed.
    pub fn is_exhausted(&self) -> bool {
        let fresh = match self.reset_in_s {
            Some(r) => r > 0,
            None => self.age_s < QUOTA_CACHE_FRESH_S,
        };
        self.pct <= QUOTA_EXHAUSTED_PCT && fresh
    }
}

/// The window `group` was last seen exhausted in, weekly first (the longer wait
/// wins when both are out). Pure: both logs are read by the caller.
pub(crate) fn exhausted_window(
    quota_raw: &str,
    runs_raw: &str,
    group: &str,
    now: u64,
) -> Option<(Window, CachedPct)> {
    [Window::Weekly, Window::Five].into_iter().find_map(|w| {
        effective_pct(quota_raw, runs_raw, group, w, now)
            .filter(|c| c.is_exhausted())
            .map(|c| (w, c))
    })
}

fn read_logs() -> (String, String) {
    (
        std::fs::read_to_string(quota_log_path()).unwrap_or_default(),
        std::fs::read_to_string(runs_log_path()).unwrap_or_default(),
    )
}

/// Disk-reading wrapper for [`exhausted_window`]. Zero latency (local file read, no
/// agy launch): lets the guard refuse a dispatch that would 429 before paying for it.
pub fn cached_exhausted(group: &str) -> Option<(Window, CachedPct)> {
    let (raw, runs) = read_logs();
    exhausted_window(&raw, &runs, group, now_unix())
}

/// The reading the guard may act on: the newest one for `group`, unless a run on
/// that group has **succeeded** since it was taken. A successful run is proof the
/// group was being served at that moment, so an older 0% — typically a 429 receipt
/// — can no longer be "likely still exhausted". Without this the guard kept
/// refusing for its full 4h freshness window after the window had refilled, and a
/// `force=true` run that completed normally did not lift it (observed 2026-09-14:
/// refused at ~120min, forced run ok, refused again at ~139min).
///
/// Invalidate rather than record: a success says nothing about *how much* is left,
/// and a made-up percentage would poison the burn estimate that reads the same log.
pub(crate) fn effective_pct(
    quota_raw: &str,
    runs_raw: &str,
    group: &str,
    window: Window,
    now: u64,
) -> Option<CachedPct> {
    let c = parse_cached_pct(quota_raw, group, window, now)?;
    (!ok_run_since(runs_raw, group, now.saturating_sub(c.age_s))).then_some(c)
}

/// Whether runs.jsonl holds a successful run billed to `group` that finished
/// strictly after `since_t`. The served receipt names the group; the requested
/// model is the fallback (a success always has a receipt, so this rarely matters).
pub(crate) fn ok_run_since(runs_raw: &str, group: &str, since_t: u64) -> bool {
    runs_raw
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|e| {
            e.get("ok").and_then(|x| x.as_bool()) == Some(true)
                && e.get("t")
                    .and_then(|x| x.as_u64())
                    .is_some_and(|t| t > since_t)
                && ["served", "model"]
                    .iter()
                    .filter_map(|k| e.get(*k).and_then(|x| x.as_str()))
                    .find_map(quota_group_of)
                    == Some(group)
        })
}

/// Pure scan of quota_log.jsonl text for the most recent `group` reading of
/// `window` at or before `now` — testable without touching the real `~/.agy-mcp`
/// state. A newer scrape that looked for `group` and
/// did not find it (`missing`) ends the scan with None: the reading behind it is no
/// longer known to be current, and the guard's posture on unknown is to let through.
/// A reading whose logged refresh time has passed is None for the same reason: the
/// page already said when the 0% would stop being true (2026-09-14: the app said
/// "refresh in 1h49m" while the guard would have held the 0% for 4h).
pub(crate) fn parse_cached_pct(
    raw: &str,
    group: &str,
    window: Window,
    now: u64,
) -> Option<CachedPct> {
    for l in raw.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else {
            continue;
        };
        let Some(t) = v.get("t").and_then(|x| x.as_u64()) else {
            continue;
        };
        let missed = v
            .get("missing")
            .and_then(|x| x.as_array())
            .is_some_and(|a| a.iter().any(|g| g.as_str() == Some(group)));
        if missed {
            return None;
        }
        let Some(g) = v.get("groups").and_then(|g| g.get(group)) else {
            continue;
        };
        if let Some(pct) = g.get(window.key()).and_then(|x| x.as_f64()) {
            let reset = g.get(window.reset_key()).and_then(|x| x.as_u64());
            if reset.is_some_and(|r| r <= now) {
                return None;
            }
            return Some(CachedPct {
                pct,
                age_s: now.saturating_sub(t),
                reset_in_s: reset.map(|r| r - now),
                from_receipt: v.get("src").and_then(|x| x.as_str()) == Some("429"),
            });
        }
    }
    None
}

/// "weekly (resets in 3d4h)" — the window and how long it stays shut. A receipt's
/// 0% says only that a 429 happened, so it names neither.
fn window_phrase(w: Window, c: &CachedPct) -> String {
    let win = if c.from_receipt { "429" } else { w.key() };
    let when = match c.reset_in_s {
        Some(r) => format!("resets in {}", fmt_dur(r)),
        None => format!("seen {} ago, reset unknown", fmt_dur(c.age_s)),
    };
    format!("{win} ({when})")
}

/// Returns a hint string if the requested model's quota group was seen exhausted recently.
/// Same judgment as the pre-flight guard (`CachedPct::is_exhausted`).
pub fn quota_probable_cause(requested_label: Option<&str>) -> Option<String> {
    let group = quota_group_of(requested_label?)?;
    let (w, c) = cached_exhausted(group)?;
    Some(eval_quota_probable_cause(group, w, &c))
}

pub(crate) fn eval_quota_probable_cause(group: &str, w: Window, c: &CachedPct) -> String {
    format!(
        "probable cause: quota — {group} {} was last seen at {:.1}% (not certain; this is a \
         correlation with the last quota reading, not a receipt from this run)",
        window_phrase(w, c),
        c.pct
    )
}

/// Which window a 429 just hit, as far as the cached readings know — appended to a
/// `QUOTA_EXHAUSTED:` failure so the caller can tell hours from days. None when
/// the only evidence is the receipt itself.
pub fn wall_window_note(group: &str) -> Option<String> {
    let (w, c) = cached_exhausted(group)?;
    (!c.from_receipt).then(|| format!("window: {group} {}", window_phrase(w, &c)))
}

/// The refusal for a dispatch to an exhausted group, or None to let it through.
/// The first line is fixed-shape — `AGY_UNAVAILABLE: <group> <weekly|5h|429>
/// (<when>) → model=<other>|none` — so a caller's rule can match it mechanically;
/// `<other>` is a concrete base name from the catalog (`labels`), see
/// [`other_quota_group`]. Pure: both logs and the catalog are read by the caller.
pub(crate) fn quota_unavailable(
    quota_raw: &str,
    runs_raw: &str,
    group: &str,
    labels: &[String],
    now: u64,
) -> Option<String> {
    let (w, c) = exhausted_window(quota_raw, runs_raw, group, now)?;
    let other = other_quota_group(group, labels);
    let other_dead = other.as_ref().and_then(|&(g, _)| {
        exhausted_window(quota_raw, runs_raw, g, now).map(|(ow, oc)| (g, ow, oc))
    });
    let to = match (other, &other_dead) {
        (Some((_, m)), None) => format!("model={m}"),
        _ => "none".to_string(),
    };
    let mut out = format!(
        "AGY_UNAVAILABLE: {group} {} → {to}\n{group} was last seen at {:.1}%.",
        window_phrase(w, &c),
        c.pct
    );
    if let Some((g, ow, oc)) = other_dead {
        out += &format!(" {g} is out too: {}.", window_phrase(ow, &oc));
    }
    out += " Pass force=true to try anyway, or run agy_diag(action=live) for a fresh reading.";
    Some(out)
}

fn fmt_pct(x: f64) -> String {
    if x < 10.0 {
        format!("{x:.1}%")
    } else {
        format!("{x:.0}%")
    }
}

/// One line for the server's `instructions`: each group's cached state, so the
/// caller knows before its first call whether agy is worth trying. Built from
/// the cache only (no quota, no latency) and dated, because it is read once per
/// session and never updated — the refusal text is the channel for later news.
/// None when there is no reading at all.
pub(crate) fn status_line(quota_raw: &str, runs_raw: &str, now: u64) -> Option<String> {
    let newest_t = quota_raw.lines().rev().find_map(|l| {
        serde_json::from_str::<serde_json::Value>(l)
            .ok()?
            .get("t")?
            .as_u64()
    })?;
    let groups: Vec<String> = KNOWN_GROUPS
        .into_iter()
        .filter_map(|g| {
            if let Some((w, c)) = exhausted_window(quota_raw, runs_raw, g, now) {
                return Some(format!("{g} ✗ {}", window_phrase(w, &c)));
            }
            let read = |w| effective_pct(quota_raw, runs_raw, g, w, now).map(|c| c.pct);
            let buckets: Vec<String> = [
                (Window::Five, read(Window::Five)),
                (Window::Weekly, read(Window::Weekly)),
            ]
            .into_iter()
            .filter_map(|(w, p)| Some(format!("{} {}", w.key(), fmt_pct(p?))))
            .collect();
            if buckets.is_empty() {
                // An optional group the plan does not have is not worth a `?`.
                return (g != ARGON_GROUP).then(|| format!("{g} ?"));
            }
            let left = group_burn(quota_raw, runs_raw, g, now)
                .as_ref()
                .and_then(weekly_runs_left)
                .map(|n| {
                    let warn = if n <= 10 { "⚠ " } else { "" };
                    format!(" {warn}~{n} runs left this week")
                })
                .unwrap_or_default();
            Some(format!("{g} {}{left}", buckets.join(" / ")))
        })
        .collect();
    Some(format!(
        "agy quota (cached {} ago): {}",
        fmt_dur(now.saturating_sub(newest_t)),
        groups.join(" | ")
    ))
}

/// Disk-reading wrapper for [`status_line`].
pub fn cached_status_line() -> Option<String> {
    let (raw, runs) = read_logs();
    status_line(&raw, &runs, now_unix())
}

/// Disk-reading wrapper for [`quota_unavailable`].
pub fn cached_quota_unavailable(group: &str) -> Option<String> {
    let (raw, runs) = read_logs();
    let labels = crate::agy::models::load_models_cache();
    quota_unavailable(&raw, &runs, group, &labels, now_unix())
}

/// The other quota group and a model billing to it, so a run refused for one group
/// doesn't leave the caller guessing which model draws from the independent bucket.
/// The model is a concrete base name from the catalog that `model=` resolves as a
/// name (`claude-sonnet-5-5`, `gemini-3.8-flash`): the newest Sonnet for the Claude
/// group (not Opus — the user's call, 2026-10-16: Sonnet is the better use of that
/// bucket) and the newest Flash-slot line for the Gemini group. None when the
/// catalog has no such model; an empty catalog (cold cache) names the alias.
pub fn other_quota_group(
    exhausted_group: &str,
    labels: &[String],
) -> Option<(&'static str, String)> {
    let is_sonnet =
        |l: &str| slot_of(l) == Some(Slot::Claude) && l.to_ascii_lowercase().contains("sonnet");
    let is_flash = |l: &str| slot_of(l) == Some(Slot::Flash);
    let (group, alias, pick): (_, _, &dyn Fn(&str) -> bool) =
        if exhausted_group == group_of_slot(Slot::Flash) {
            (group_of_slot(Slot::Claude), "sonnet", &is_sonnet)
        } else if exhausted_group == group_of_slot(Slot::Claude) || exhausted_group == ARGON_GROUP {
            (group_of_slot(Slot::Flash), "flash", &is_flash)
        } else {
            return None;
        };
    if labels.is_empty() {
        return Some((group, alias.to_string()));
    }
    let model = labels
        .iter()
        .filter(|l| pick(l))
        .max_by_key(|l| crate::agy::models::model_version(l))
        .map(|l| crate::agy::models::base_slug(l))?;
    Some((group, model))
}

/// Words the alternative-group hint, or None when the other group's own cached
/// reading is itself a fresh exhausted one — pointing the caller at a second dead
/// bucket would waste the retry it's meant to save.
pub fn format_quota_alternative(
    other_group: &str,
    model: &str,
    reading: Option<CachedPct>,
) -> Option<String> {
    let status = match reading {
        Some(c) => {
            if c.is_exhausted() {
                return None;
            }
            if c.pct > QUOTA_EXHAUSTED_PCT {
                format!(" last seen {:.0}%", c.pct)
            } else {
                String::new()
            }
        }
        None => String::new(),
    };

    Some(format!(
        "other quota group {other_group}{status} — retry or resume with model={model}"
    ))
}

/// Pure decision: names the OTHER quota group and a model that bills to it,
/// when that group's cached reading is not exhausted or is unknown.
/// Without this hint, a caller hitting a quota wall has to guess which other
/// model line draws from an independent bucket.
pub fn quota_alternative(
    exhausted_group: &str,
    quota_raw: &str,
    runs_raw: &str,
    labels: &[String],
    now: u64,
) -> Option<String> {
    let (other_group, model) = other_quota_group(exhausted_group, labels)?;
    // A weekly wall on the other group is as dead as a 5h one.
    if exhausted_window(quota_raw, runs_raw, other_group, now).is_some() {
        return None;
    }
    let reading = effective_pct(quota_raw, runs_raw, other_group, Window::Five, now);
    format_quota_alternative(other_group, &model, reading)
}

/// Disk-reading wrapper for `quota_alternative`.
pub fn cached_quota_alternative(exhausted_group: &str) -> Option<String> {
    let (raw, runs) = read_logs();
    let labels = crate::agy::models::load_models_cache();
    quota_alternative(exhausted_group, &raw, &runs, &labels, now_unix())
}

/// Estimate burn rate for both quota groups (GEMINI, and Claude's "CLAUDE AND
/// GPT") from quota snapshots + the runs recorded between them. A "目安" only:
/// the 5h window is rolling (refills over time) and percentages are
/// integer-coarse. Lets a caller see Claude's burn before spending sonnet/opus
/// quota, not just Gemini's.
fn burn_summary() -> Option<String> {
    let (raw, runs) = read_logs();
    let now = now_unix();
    let lines: Vec<String> = KNOWN_GROUPS
        .into_iter()
        .filter_map(|group| {
            let est = group_burn(&raw, &runs, group, now)?;
            // A group with no runs at all in the window is silence, not "not
            // enough" — the caller never asked it to spend that quota.
            (est.runs_seen > 0).then(|| format_burn(group, &est))
        })
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// How far back the burn estimate gathers snapshot pairs.
const BURN_WINDOW_S: u64 = 24 * 3600;
/// Below this many measured runs no rate is printed at all. The old code divided
/// by whatever it had — including 1 — and stated the result as fact.
const MIN_BURN_RUNS: u64 = 5;

/// A quota snapshot reduced to what the burn estimate needs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BurnSnap {
    pub t: u64,
    pub weekly: Option<f64>,
    pub five: Option<f64>,
}

/// A run reduced likewise. Only runs whose *served* model belongs to the group
/// reach here (the caller filters).
#[derive(Debug, Clone, Copy)]
pub(crate) struct BurnRun {
    pub t: u64,
    pub ctx: u64,
}

/// What the log can actually support saying. Each rate carries the run count it
/// was measured over, so the formatter — not the arithmetic — decides whether
/// there is enough evidence to print a number.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct BurnEstimate {
    /// (%/run, runs it was measured over)
    pub weekly: Option<(f64, u64)>,
    pub five: Option<(f64, u64)>,
    pub pct_per_1k_ctx: Option<f64>,
    /// Newest 5h% remaining, for the "runs left" projection.
    pub five_now: Option<f64>,
    /// Newest weekly% remaining, likewise — a heavy week empties this one first
    /// (gen3dasset, 2026-09-22: ~1.1%/run, gone in ~88 runs).
    pub weekly_now: Option<f64>,
    /// Runs in the window at all, measurable or not — what "not enough" reports.
    pub runs_seen: u64,
}

/// Aggregate burn across every snapshot pair in the window instead of the last
/// two, and drop intervals we cannot attribute.
///
/// Two things made the old estimate lie, and both are fixed here:
///
/// 1. **It used exactly one interval.** With one run between the last two
///    snapshots it divided by 1 and printed the result as a rate — observed
///    extrapolating `5h -74.21%/run` → "~0 runs left" while 53% remained
///    (2026-08-03), and "~20 runs left" moments before a live reading of 97%
///    (2026-08-13).
/// 2. **The 5h window refills.** An interval where the percentage went UP was
///    clamped to zero consumption and its runs still counted, which drags the
///    per-run rate toward zero. Such an interval is now dropped whole: the runs
///    in it cost something we cannot measure, and pretending they cost nothing
///    is how a rate gets quietly wrong in the *other* direction.
///
/// Pure: both logs are read by the caller. `None` = not even two snapshots.
pub(crate) fn burn_estimate(
    snaps: &[BurnSnap],
    runs: &[BurnRun],
    now: u64,
) -> Option<BurnEstimate> {
    let recent: Vec<&BurnSnap> = snaps
        .iter()
        .filter(|s| now.saturating_sub(s.t) <= BURN_WINDOW_S)
        .collect();
    if recent.len() < 2 {
        return None;
    }
    let (t_first, t_last) = (recent[0].t, recent[recent.len() - 1].t);
    let (mut w_drop, mut w_runs) = (0.0f64, 0u64);
    let (mut f_drop, mut f_runs, mut f_toks) = (0.0f64, 0u64, 0u64);
    for pair in recent.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        // Half-open (a.t, b.t] so a run on a boundary isn't counted in both
        // adjacent intervals.
        let in_iv: Vec<&BurnRun> = runs.iter().filter(|r| r.t > a.t && r.t <= b.t).collect();
        if in_iv.is_empty() {
            continue;
        }
        if let (Some(x), Some(y)) = (a.weekly, b.weekly) {
            if y <= x {
                w_drop += x - y;
                w_runs += in_iv.len() as u64;
            }
        }
        if let (Some(x), Some(y)) = (a.five, b.five) {
            if y <= x {
                f_drop += x - y;
                f_runs += in_iv.len() as u64;
                f_toks += in_iv.iter().map(|r| r.ctx).sum::<u64>();
            }
        }
    }
    Some(BurnEstimate {
        weekly: (w_runs > 0).then(|| (w_drop / w_runs as f64, w_runs)),
        five: (f_runs > 0).then(|| (f_drop / f_runs as f64, f_runs)),
        pct_per_1k_ctx: (f_toks > 0).then(|| f_drop / (f_toks as f64 / 1000.0)),
        five_now: recent[recent.len() - 1].five,
        weekly_now: recent[recent.len() - 1].weekly,
        runs_seen: runs
            .iter()
            .filter(|r| r.t > t_first && r.t <= t_last)
            .count() as u64,
    })
}

/// Render one group's estimate. Says "not enough" rather than guessing — the
/// output was the problem, not the arithmetic: a caller reading a confident
/// `~0 runs left` has no way to know it came from a single sample.
pub(crate) fn format_burn(group: &str, est: &BurnEstimate) -> String {
    let weekly = est.weekly.filter(|(_, n)| *n >= MIN_BURN_RUNS);
    let five = est.five.filter(|(_, n)| *n >= MIN_BURN_RUNS);
    if weekly.is_none() && five.is_none() {
        return format!(
            "  burn ({group}): not enough measured runs (n={}, need {MIN_BURN_RUNS}) — no estimate",
            est.runs_seen
        );
    }
    let n = weekly.or(five).map(|(_, n)| n).unwrap_or(0);
    let mut s = format!("  burn ({group}, {n} runs):");
    if let Some((per, _)) = weekly {
        s += &format!(" weekly -{per:.2}%/run");
        if let Some(left) = weekly_runs_left(est) {
            s += &format!(" (~{left} runs left this week)");
        }
    }
    if let Some((per, _)) = five {
        s += &format!(" · 5h -{per:.2}%/run");
        if let Some(pk) = est.pct_per_1k_ctx {
            s += &format!(" (~{pk:.2}%/1k ctx)");
        }
        // Only project from a rate we were willing to print, and only while it is
        // positive — a zero rate would divide into "infinite runs left".
        if per > 0.0 {
            if let Some(now_pct) = est.five_now {
                s += &format!(" · ~{} runs left in 5h (rolling)", (now_pct / per) as u64);
            }
        }
    }
    s
}

/// Runs the weekly bucket has left at the measured rate, or None when the rate
/// rests on too few runs (or is zero, which would divide into "infinite").
pub(crate) fn weekly_runs_left(est: &BurnEstimate) -> Option<u64> {
    let (per, n) = est.weekly?;
    (n >= MIN_BURN_RUNS && per > 0.0).then_some(())?;
    Some((est.weekly_now? / per) as u64)
}

/// Burn estimate for one quota group from the raw logs, counting only runs whose
/// *served* model (the actual receipt, not the request) bills to it. Pure; `None`
/// when the window holds fewer than two snapshots.
pub(crate) fn group_burn(
    quota_raw: &str,
    runs_raw: &str,
    group: &str,
    now: u64,
) -> Option<BurnEstimate> {
    let snaps: Vec<BurnSnap> = quota_raw
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            let g = v.get("groups")?.get(group)?;
            Some(BurnSnap {
                t: v.get("t")?.as_u64()?,
                // Percentages are logged as f64 (2-decimal page precision);
                // as_f64 also reads older integer-logged entries fine.
                weekly: g.get("weekly").and_then(|x| x.as_f64()),
                five: g.get("5h").and_then(|x| x.as_f64()),
            })
        })
        .collect();
    let runs: Vec<BurnRun> = runs_raw
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| {
            quota_group_of(e.get("served").and_then(|v| v.as_str()).unwrap_or("")) == Some(group)
        })
        .filter_map(|e| {
            Some(BurnRun {
                t: e.get("t")?.as_u64()?,
                ctx: e.get("ctx").and_then(|v| v.as_u64()).unwrap_or(0),
            })
        })
        .collect();
    burn_estimate(&snaps, &runs, now)
}

/// Unix seconds from an ISO 8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`, optional
/// fractional seconds ignored) — `/usage`'s `reset_time`. No date crate for one field.
pub(crate) fn iso8601_utc_to_unix(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let base = if let Some((head, frac)) = s.split_once('.') {
        if frac.is_empty() || !frac.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        head
    } else {
        s
    };

    if base.len() != 19 {
        return None;
    }
    let b = base.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    for (i, &byte) in b.iter().enumerate() {
        if matches!(i, 4 | 7 | 10 | 13 | 16) {
            continue;
        }
        if !byte.is_ascii_digit() {
            return None;
        }
    }

    let year: i64 = base[0..4].parse().ok()?;
    let month: u32 = base[5..7].parse().ok()?;
    let day: u32 = base[8..10].parse().ok()?;
    let hour: u64 = base[11..13].parse().ok()?;
    let min: u64 = base[14..16].parse().ok()?;
    let sec: u64 = base[17..19].parse().ok()?;

    if year < 1970 || !(1..=12).contains(&month) {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let max_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if day < 1 || day > max_days || hour >= 24 || min >= 60 || sec >= 60 {
        return None;
    }

    // Days from civil (Howard Hinnant algorithm, civil calendar to days since 1970-01-01)
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32;
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;
    if days < 0 {
        return None;
    }
    Some((days as u64) * 86_400 + hour * 3600 + min * 60 + sec)
}

/// Quota groups from `agy -p /usage --output-format json`. Group names are
/// normalized to `KNOWN_GROUPS` ("Gemini Models" → "GEMINI"); each reset becomes
/// seconds from `now`, so `quota_log_entry` stores it exactly as before.
pub(crate) fn parse_usage_json(raw: &str, now: u64) -> Option<Vec<QGroup>> {
    let val: serde_json::Value = serde_json::from_str(raw).ok()?;
    let groups = val.get("command")?.get("data")?.get("groups")?.as_array()?;

    let mut result = Vec::new();
    for group in groups {
        let raw_name = group.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let trimmed = raw_name.trim();
        let upper = trimmed.to_uppercase();
        let name = upper.strip_suffix(" MODELS").unwrap_or(&upper).to_string();

        let mut weekly = None;
        let mut five = None;
        let mut weekly_reset = None;
        let mut five_reset = None;

        if let Some(buckets) = group.get("buckets").and_then(|v| v.as_array()) {
            for bucket in buckets {
                let Some(window) = bucket.get("window").and_then(|v| v.as_str()) else {
                    continue;
                };
                let Some(frac) = bucket.get("remaining_fraction").and_then(|v| v.as_f64()) else {
                    continue;
                };
                let percent = (frac * 10000.0).round() / 100.0;
                // A disabled bucket ("the 5-hour limit does not currently apply") still
                // carries a fraction; showing it as 100% reads as headroom that is not there.
                let display = if bucket.get("disabled").and_then(|v| v.as_bool()) == Some(true) {
                    "disabled".to_string()
                } else {
                    format!("{percent:.0}%")
                };
                let reset = bucket
                    .get("reset_time")
                    .and_then(|v| v.as_str())
                    .and_then(iso8601_utc_to_unix)
                    .map(|t| t.saturating_sub(now));

                match window {
                    "weekly" => {
                        weekly = Some((percent, display));
                        weekly_reset = reset;
                    }
                    "5h" => {
                        five = Some((percent, display));
                        five_reset = reset;
                    }
                    _ => {}
                }
            }
        }

        result.push(QGroup {
            name,
            weekly,
            five,
            weekly_reset,
            five_reset,
        });
    }

    Some(result)
}

/// The AI-credit balance from `agy -p /credits --output-format json`.
pub(crate) fn parse_credits_json(raw: &str) -> Option<u64> {
    let val: serde_json::Value = serde_json::from_str(raw).ok()?;
    val.get("command")?
        .get("data")?
        .get("remaining_credits")?
        .as_u64()
}

/// One bucket for the live summary. The refresh time rides along on the rolling 5h
/// bucket always, and on the weekly one only once it is spent — a 109h countdown on a
/// healthy weekly bucket is noise.
fn bucket_display(b: &Option<(f64, String)>, reset: Option<u64>, always: bool) -> String {
    let Some((p, d)) = b else {
        return "?".into();
    };
    match reset {
        Some(r) if d != "disabled" && (always || *p <= 1.0) => {
            format!("{d} (refresh {})", fmt_dur(r))
        }
        _ => d.clone(),
    }
}

/// Render parsed groups into the concise, token-frugal quota summary. Returns
/// None if no group carried a limit (caller falls back to the proxy).
pub(super) fn format_usage(groups: &[QGroup], credits: Option<u64>) -> Option<String> {
    let qg: Vec<&QGroup> = groups
        .iter()
        .filter(|g| g.weekly.is_some() || g.five.is_some())
        .collect();
    if qg.is_empty() {
        return None;
    }
    let width = qg.iter().map(|g| g.name.len()).max().unwrap_or(0);
    let mut out = vec!["agy サブスク quota（一次ソース: /usage）".to_string()];
    for g in qg {
        out.push(format!(
            "  {:<width$}  weekly {} · 5h {}",
            g.name,
            bucket_display(&g.weekly, g.weekly_reset, false),
            bucket_display(&g.five, g.five_reset, true),
            width = width
        ));
    }
    // A zero balance is the normal state on a subscription — only a real one is news.
    if let Some(c) = credits.filter(|c| *c > 0) {
        out.push(format!("  AI credits: {c}"));
    }
    out.push("注: weekly/5hが従量ゼロ枠の本体。AI creditsは枠切れ時のみ消費。".to_string());
    Some(out.join("\n"))
}

/// Parse + log + format the quota panel from `/usage` JSON. None if no quota
/// group was found (caller falls back to the proxy).
pub(super) fn quota_from_json(usage: &str, credits: Option<u64>, now: u64) -> Option<String> {
    let groups = parse_usage_json(usage, now)?;
    let mut s = format_usage(&groups, credits)?;
    let missing = missing_groups(&groups);
    log_quota(&groups, &missing); // accrue burn-rate history (no extra quota)
    for g in &missing {
        s.push('\n');
        s.push_str(&missing_row_note(g));
    }
    for g in unguarded_groups(&groups) {
        s.push('\n');
        s.push_str(&unguarded_group_note(g));
    }
    Some(s)
}

/// stdout of `agy -p <command> --output-format json` for one of agy's read-only
/// slash commands (`/usage`, `/credits`): answered with no agent turn, no quota and
/// no conversation, from any cwd. Spawned like every agy child — stdin closed,
/// windowless (invariant #1).
fn slash_command_json(command: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let bin = crate::agy::paths::agy_bin();
    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["-p", command, "--output-format", "json"])
        .stdin(std::process::Stdio::null())
        .envs(crate::nest::spawned_env());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd
        .output()
        .with_context(|| format!("running `{bin} -p {command}`"))?;
    if !out.status.success() {
        anyhow::bail!("`agy -p {command}` exited {:?}", out.status.code());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Live subscription quota from `/usage` (plus the `/credits` balance). Falls back
/// to the offline activity proxy if the command or the parse fails.
pub async fn live_quota() -> String {
    let fetched = tokio::task::spawn_blocking(|| {
        let usage = slash_command_json("/usage")?;
        let credits = slash_command_json("/credits")
            .ok()
            .and_then(|c| parse_credits_json(&c));
        anyhow::Ok((usage, credits))
    })
    .await;
    match fetched {
        Ok(Ok((usage, credits))) => match quota_from_json(&usage, credits, now_unix()) {
            Some(s) => s,
            None => format!(
                "{}\n(/usage の JSON を解釈できず proxy にフォールバック)",
                quota_info().await
            ),
        },
        Ok(Err(e)) => format!(
            "{}\n(/usage 失敗: {e:#} — proxy にフォールバック)",
            quota_info().await
        ),
        Err(_) => format!(
            "{}\n(/usage 失敗: proxy にフォールバック)",
            quota_info().await
        ),
    }
}

/// Honest usage info. The Antigravity CLI exposes NO quota/usage command and no
/// usage file (verified on 1.0.8), so we report a usage *proxy* (conversation
/// volume + last activity + currently-selected model) and point at the only
/// authoritative source: the Antigravity app.
pub async fn quota_info() -> String {
    let mut lines = vec!["agy subscription usage (proxy — see note)".to_string()];

    // Conversation count + most-recent activity as an activity proxy.
    let mut count = 0usize;
    let mut newest: Option<SystemTime> = None;
    if let Ok(rd) = std::fs::read_dir(brain_dir()) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                count += 1;
                if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                    newest = Some(newest.map_or(m, |n| n.max(m)));
                }
            }
        }
    }
    lines.push(format!("  conversations on disk : {count}"));
    let last = newest
        .and_then(|t| t.elapsed().ok())
        .map(|d| format!("{}m ago", d.as_secs() / 60))
        .unwrap_or_else(|| "n/a".into());
    lines.push(format!("  last activity         : {last}"));

    // Currently-selected model, from the newest transcript receipt.
    let selected = newest_conv_dir()
        .and_then(|c| read_transcript(&c).ok())
        .and_then(|(_, m)| m)
        .unwrap_or_else(|| "unknown".into());
    lines.push(format!("  selected model        : {selected}"));

    lines.push(
        "NOTE: exact remaining quota is NOT exposed by the agy CLI. Check the \
         Antigravity app/account page for your real AI Pro/Ultra balance."
            .into(),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // The older tests compare (pct, age) pairs; these shadow the glob import so they
    // keep reading that way. The refresh-time tests call `super::` directly.
    fn parse_cached_five_pct(raw: &str, group: &str, now: u64) -> Option<(f64, u64)> {
        parse_cached_pct(raw, group, Window::Five, now).map(|c| (c.pct, c.age_s))
    }
    fn effective_five_pct(quota: &str, runs: &str, group: &str, now: u64) -> Option<(f64, u64)> {
        effective_pct(quota, runs, group, Window::Five, now).map(|c| (c.pct, c.age_s))
    }

    /// The app said "refresh in 1h49m" for a 0% the guard would have held for 4h.
    #[test]
    fn a_zero_stops_blocking_at_its_refresh_time() {
        let log = "{\"t\":3000,\"groups\":{\"GEMINI\":{\"5h\":0.0,\"5h_reset\":8700}}}";
        let before = parse_cached_pct(log, "GEMINI", Window::Five, 5000).unwrap();
        assert_eq!(
            (before.pct, before.age_s, before.reset_in_s),
            (0.0, 2000, Some(3700))
        );
        assert_eq!(parse_cached_pct(log, "GEMINI", Window::Five, 8700), None);
        // A reading logged before refresh times were kept still reads, with no reset.
        let legacy = "{\"t\":3000,\"groups\":{\"GEMINI\":{\"5h\":0.0}}}";
        assert_eq!(
            parse_cached_pct(legacy, "GEMINI", Window::Five, 20_000).map(|c| c.reset_in_s),
            Some(None)
        );
    }

    /// gen3dasset, 2026-09-22 (and this repo, 2026-09-29): an exhausted week logs
    /// `weekly: 0` with no 5h bucket at all, and the 5h-only guard let every run
    /// through to a 429.
    const WEEK_OUT: &str = concat!(
        "{\"t\":1000,\"groups\":{\"GEMINI\":{\"5h\":61.4,\"weekly\":1.8}}}\n",
        "{\"t\":2000,\"groups\":{\"GEMINI\":{\"weekly\":0.0,\"weekly_reset\":300000},",
        "\"CLAUDE AND GPT\":{\"5h\":100.0,\"weekly\":13.2}}}"
    );

    /// `agy models` on 2026-10-16 (abridged).
    fn cat() -> Vec<String> {
        [
            "gemini-4-argon-medium",
            "gemini-4-argon-high",
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-medium",
            "claude-opus-5-5-medium",
            "claude-sonnet-5-5-low",
            "claude-sonnet-5-5-medium",
            "gpt-oss-120b-medium",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn an_exhausted_week_refuses_with_the_window_and_the_other_group() {
        let (w, c) = exhausted_window(WEEK_OUT, "", "GEMINI", 3000).expect("week is out");
        assert_eq!(w, Window::Weekly);
        assert_eq!(c.reset_in_s, Some(297_000));
        let msg = quota_unavailable(WEEK_OUT, "", "GEMINI", &cat(), 3000).unwrap();
        let head = msg.lines().next().unwrap();
        assert_eq!(
            head,
            "AGY_UNAVAILABLE: GEMINI weekly (resets in 3d10h) → model=claude-sonnet-5-5"
        );
        assert!(msg.contains("force=true"), "{msg}");
        // The other group is alive, so it is not refused.
        assert_eq!(
            quota_unavailable(WEEK_OUT, "", "CLAUDE AND GPT", &cat(), 3000),
            None
        );
        // Nor is it offered once its own week is out too.
        let both = format!(
            "{WEEK_OUT}\n{{\"t\":2500,\"groups\":{{\"CLAUDE AND GPT\":{{\"weekly\":0.0,\"weekly_reset\":50000}}}}}}"
        );
        let msg = quota_unavailable(&both, "", "GEMINI", &cat(), 3000).unwrap();
        assert!(
            msg.starts_with("AGY_UNAVAILABLE: GEMINI weekly (resets in 3d10h) → none"),
            "{msg}"
        );
        assert!(
            msg.contains("CLAUDE AND GPT is out too: weekly (resets in 13h"),
            "{msg}"
        );
        assert_eq!(quota_alternative("GEMINI", &both, "", &cat(), 3000), None);
        // Its refresh time passing lets it through again.
        assert_eq!(exhausted_window(WEEK_OUT, "", "GEMINI", 300_000), None);
    }

    #[test]
    fn a_receipt_zero_names_no_window() {
        let log = "{\"t\":1000,\"groups\":{\"GEMINI\":{\"5h\":0.0}},\"src\":\"429\"}";
        let msg = quota_unavailable(log, "", "GEMINI", &cat(), 1600).unwrap();
        assert!(
            msg.starts_with(
                "AGY_UNAVAILABLE: GEMINI 429 (seen 10m ago, reset unknown) → model=claude-sonnet-5-5"
            ),
            "{msg}"
        );
    }

    #[test]
    fn status_line_reads_the_cache_for_both_groups() {
        let line = status_line(WEEK_OUT, "", 2600).unwrap();
        assert_eq!(
            line,
            "agy quota (cached 10m ago): GEMINI ✗ weekly (resets in 3d10h) | \
             CLAUDE AND GPT 5h 100% / weekly 13%"
        );
        assert_eq!(status_line("", "", 2600), None);
    }

    #[test]
    fn agy_1_2_wording_is_a_quota_wall() {
        assert!(is_quota_wall(
            "error: RESOURCE_EXHAUSTED (code 429): Resource has been exhausted (e.g. check quota)."
        ));
        assert!(is_quota_wall("[429] rate limited"));
        assert!(!is_quota_wall(
            "API error (attempt 3): UNAVAILABLE (code 503)"
        ));
        assert!(!is_quota_wall("✓ task-429: ok (exit=0)"));
    }

    #[test]
    fn an_agy_error_receipt_naming_429_is_a_quota_wall() {
        assert!(is_quota_wall(
            "agy exited with Some(1): AGY_ERROR short_error=rate limited · http_status=429"
        ));
        assert!(is_quota_wall(r#"AGY_ERROR: {"status":"X","code":429}"#));
        // Not without the receipt, not another status, not a longer number.
        assert!(!is_quota_wall("exit=429 from the test"));
        assert!(!is_quota_wall(
            "AGY_ERROR short_error=boom · http_status=500"
        ));
        assert!(!is_quota_wall("AGY_ERROR short_error=boom · pid=4290"));
    }

    /// Same root cause as the row-splitting bug below, in the other direction: a
    /// failed run has `served: ""`, and `slot_mismatch` correctly refuses to judge
    /// an unclassifiable side — so counting it at all filed it as a MATCH and
    /// diluted the rate. A run that produced no receipt is not evidence either way.
    #[test]
    fn a_run_with_no_served_receipt_is_not_evidence_of_a_match() {
        let row = |served: &str| serde_json::json!({"model": "gemini-3.1-pro-high", "served": served, "ok": !served.is_empty()});
        let stats = requested_mismatch_stats(&[
            row("Gemini 3.1 Pro (High)"),
            row("Gemini 3.6 Flash (Low)"), // a real silent downgrade
            row(""),                       // failed before agy served anything
            row(""),
        ]);
        assert_eq!(
            stats.get("gemini-3.1-pro-high"),
            Some(&(1u32, 2u32)),
            "1 mismatch out of the 2 runs that actually reported a model"
        );
    }

    /// The bug this guards: a failed run has `served: ""`, so keying on
    /// `served.unwrap_or(model)` filed its successes under agy's display label
    /// and its failures under the requested slug — two rows for one model, one
    /// 100% ok and one 0%, and no row anywhere showing the real success rate.
    #[test]
    fn ok_and_failed_runs_of_one_model_share_a_row() {
        let mut entries: Vec<serde_json::Value> = (0..3)
            .map(|_| {
                serde_json::json!({
                    "tool": "agy_sub", "model": "gemini-3.6-flash-low",
                    "served": "Gemini 3.6 Flash (Low)",
                    "steps": 4, "ctx": 20000, "elapsed_s": 30.0, "ok": true,
                })
            })
            .collect();
        entries.push(serde_json::json!({
            "tool": "agy_sub", "model": "gemini-3.6-flash-low",
            "served": "", "steps": 0, "ctx": 0, "elapsed_s": 5.0, "ok": false,
        }));

        let rows = model_stat_rows(&entries);
        assert_eq!(rows.len(), 1, "one model must be one row: {rows:#?}");
        assert!(rows[0].contains("gemini-3.6-flash-low"), "{}", rows[0]);
        assert!(rows[0].contains("n=4 ok=75%"), "{}", rows[0]);
        // The failed run contributes no steps/ctx/elapsed sample.
        assert!(rows[0].contains("steps med 4 (p90 4)"), "{}", rows[0]);
    }

    #[test]
    fn distinct_models_stay_distinct() {
        let entries = vec![
            serde_json::json!({"model": "gemini-3.6-flash-low", "served": "", "ok": false}),
            serde_json::json!({"model": "claude-sonnet-4-6", "served": "", "ok": true}),
            serde_json::json!({"model": "claude-opus-4-6-thinking", "served": "", "ok": true}),
        ];
        assert_eq!(model_stat_rows(&entries).len(), 3);
    }

    #[test]
    fn test_stats_group_label() {
        // Requested wins over served, so a failure lands with its successes.
        assert_eq!(
            stats_group_label(Some("gemini-3.1-pro-high"), Some("Gemini 3.1 Pro (High)")),
            "gemini-3.1-pro-high"
        );
        assert_eq!(
            stats_group_label(Some("gemini-3.1-pro-high"), Some("")),
            "gemini-3.1-pro-high"
        );
        // No explicit request (bare keep-model resume) → the served receipt names it.
        assert_eq!(
            stats_group_label(Some("default"), Some("Gemini 3.6 Flash (Low)")),
            "Gemini 3.6 Flash (Low)"
        );
        // Implausible served capture is not a label; nothing usable → "default".
        assert_eq!(stats_group_label(Some("default"), Some("a\nb")), "default");
        assert_eq!(stats_group_label(None, None), "default");
    }

    #[test]
    fn test_canon_model() {
        // Label and slug spellings of one model collapse; distinct models don't.
        assert_eq!(
            canon_model("Gemini 3.6 Flash (Low)"),
            canon_model("gemini-3.6-flash-low")
        );
        assert_eq!(
            canon_model("Claude Sonnet 4.6"),
            canon_model("claude-sonnet-4-6")
        );
        assert_ne!(
            canon_model("claude-sonnet-4-6"),
            canon_model("claude-opus-4-6")
        );
        assert_ne!(
            canon_model("gemini-3.6-flash-low"),
            canon_model("gemini-3.6-flash-high")
        );
        assert_ne!(
            canon_model("gemini-3.5-pro-high"),
            canon_model("gemini-3.6-pro-high")
        );
    }

    #[test]
    fn cached_quota_picks_latest_matching_group() {
        let log = "\
{\"t\":1000,\"groups\":{\"GEMINI\":{\"5h\":50.0},\"CLAUDE AND GPT\":{\"5h\":10.0}}}
{\"t\":2000,\"groups\":{\"GEMINI\":{\"5h\":40.0}}}
{\"t\":3000,\"groups\":{\"CLAUDE AND GPT\":{\"5h\":0.0}}}";
        // GEMINI: latest entry that carries it is t=2000, 1500s before "now"=3500.
        assert_eq!(
            parse_cached_five_pct(log, "GEMINI", 3500),
            Some((40.0, 1500))
        );
        // CLAUDE AND GPT: the t=3000 entry is the latest, 500s ago.
        assert_eq!(
            parse_cached_five_pct(log, "CLAUDE AND GPT", 3500),
            Some((0.0, 500))
        );
        assert_eq!(parse_cached_five_pct(log, "NOPE", 3500), None);
        assert_eq!(parse_cached_five_pct("", "GEMINI", 3500), None);
    }

    #[test]
    fn a_429_receipt_reads_back_as_an_exhausted_reading() {
        // record_quota_wall writes exactly this shape; the guard's own reader must
        // see it as 0% for the right group, so the two stay in step.
        let log = "{\"t\":3000,\"groups\":{\"GEMINI\":{\"5h\":0.0}},\"src\":\"429\"}";
        assert_eq!(parse_cached_five_pct(log, "GEMINI", 3060), Some((0.0, 60)));
        // ...and it must not be mistaken for a reading about the other group.
        assert_eq!(parse_cached_five_pct(log, "CLAUDE AND GPT", 3060), None);
    }

    /// The 2026-09-14 sequence: a 429 wrote GEMINI 0%, the window refilled, a
    /// `force=true` flash run completed normally — and the next dispatch was refused
    /// again, because nothing the success did reached the guard's reader.
    #[test]
    fn a_successful_run_after_a_429_clears_the_guard() {
        let quota = "{\"t\":3000,\"groups\":{\"GEMINI\":{\"5h\":0.0}},\"src\":\"429\"}";
        let now = 3000 + 139 * 60;
        let ok_flash = |t: u64| {
            format!("{{\"t\":{t},\"model\":\"gemini-3.8-flash-medium\",\"served\":\"Gemini 3.8 Flash (Medium)\",\"ok\":true}}")
        };
        // No run since → the 0% still stands.
        assert_eq!(
            effective_five_pct(quota, "", "GEMINI", now),
            Some((0.0, 139 * 60))
        );
        // A GEMINI run succeeded after the receipt → no longer blocks.
        assert_eq!(
            effective_five_pct(quota, &ok_flash(4200), "GEMINI", now),
            None
        );

        // What does NOT clear it: a success from before the receipt, a failed run
        // after it, and a success on the other group.
        let not_evidence = [
            ok_flash(2900),
            "{\"t\":4200,\"model\":\"gemini-3.8-flash-medium\",\"served\":\"\",\"ok\":false}".into(),
            "{\"t\":4200,\"model\":\"claude-opus-4-6-thinking\",\"served\":\"Claude Opus 4.6 (Thinking)\",\"ok\":true}".into(),
        ]
        .join("\n");
        assert_eq!(
            effective_five_pct(quota, &not_evidence, "GEMINI", now),
            Some((0.0, 139 * 60))
        );
        // A newer 429 after the success re-arms the guard.
        let rearmed = format!(
            "{quota}\n{{\"t\":5000,\"groups\":{{\"GEMINI\":{{\"5h\":0.0}}}},\"src\":\"429\"}}"
        );
        assert_eq!(
            effective_five_pct(&rearmed, &ok_flash(4200), "GEMINI", 5060),
            Some((0.0, 60))
        );
    }

    /// The other half of 2026-09-14: `agy_diag(live)` read only the CLAUDE AND
    /// GPT group, logged it, and the GEMINI 0% behind it stayed authoritative.
    #[test]
    fn a_partial_reading_does_not_keep_a_stale_zero_authoritative() {
        let claude_only = r#"{"command":{"data":{"groups":[{"name":"Claude and GPT models","buckets":[
            {"window":"weekly","remaining_fraction":0.7571,"reset_time":"1970-01-02T00:00:00Z"},
            {"window":"5h","remaining_fraction":0.2714,"reset_time":"1970-01-01T02:00:00Z"}]}]}}}"#;
        let groups = parse_usage_json(claude_only, 3600).expect("parses");
        let missing = missing_groups(&groups);
        assert_eq!(missing, vec!["GEMINI"]);
        assert!(missing_row_note("GEMINI").contains("GEMINI row not found in usage screen"));

        let entry = quota_log_entry(&groups, &missing, 3600).expect("logged");
        let log = format!(
            "{{\"t\":3000,\"groups\":{{\"GEMINI\":{{\"5h\":0.0}}}},\"src\":\"429\"}}\n{entry}"
        );
        assert_eq!(parse_cached_five_pct(&log, "GEMINI", 3700), None);
        // The group the reading did see still reads normally.
        assert_eq!(
            parse_cached_five_pct(&log, "CLAUDE AND GPT", 3700),
            Some((27.14, 100))
        );
        // A full reading reports nothing missing, and a later reading counts again.
        let full = parse_usage_json(USAGE_JSON, 3600).expect("parses");
        assert!(missing_groups(&full).is_empty());
        let log = format!(
            "{log}\n{{\"t\":3650,\"groups\":{{\"GEMINI\":{{\"5h\":0.5}}}},\"src\":\"429\"}}"
        );
        assert_eq!(parse_cached_five_pct(&log, "GEMINI", 3700), Some((0.5, 50)));
    }

    #[test]
    fn the_quota_panel_renders_from_usage_json() {
        let now = iso8601_utc_to_unix("2026-10-03T09:00:00Z").unwrap();
        let groups = parse_usage_json(USAGE_JSON, now).expect("parses");
        let disp = format_usage(&groups, Some(0)).expect("renders");
        assert!(disp.contains("GEMINI") && disp.contains("CLAUDE AND GPT"));
        assert!(disp.contains("weekly 47%"));
        assert!(disp.contains("5h 73% (refresh"));
        assert!(!disp.contains("AI credits:"), "a zero balance stays quiet");
        assert!(format_usage(&groups, Some(12))
            .unwrap()
            .contains("AI credits: 12"));
    }

    #[test]
    fn quota_refresh_only_when_missing_or_old() {
        assert!(quota_cache_is_stale(None, 10_000));
        assert!(quota_cache_is_stale(
            Some(10_000 - QUOTA_REFRESH_AFTER_S - 1),
            10_000
        ));
        assert!(!quota_cache_is_stale(
            Some(10_000 - QUOTA_REFRESH_AFTER_S + 1),
            10_000
        ));
        assert!(!quota_cache_is_stale(Some(10_000), 10_000));
        // A clock that went backwards must not read as infinitely stale.
        assert!(!quota_cache_is_stale(Some(20_000), 10_000));
    }

    #[test]
    fn group_of_slot_maps_correctly() {
        assert_eq!(group_of_slot(Slot::Claude), "CLAUDE AND GPT");
        assert_eq!(group_of_slot(Slot::Pro), "GEMINI");
        assert_eq!(group_of_slot(Slot::Flash), "GEMINI");
    }

    #[test]
    fn quota_probable_cause_evaluates_correctly() {
        assert_eq!(quota_probable_cause(None), None);
        assert_eq!(quota_probable_cause(Some("Unknown Model Label")), None);

        let c = |pct, age_s, reset_in_s| CachedPct {
            pct,
            age_s,
            reset_in_s,
            from_receipt: false,
        };
        let hint = eval_quota_probable_cause("GEMINI", Window::Five, &c(0.0, 300, None));
        assert!(
            hint.contains("GEMINI 5h (seen 5m ago, reset unknown) was last seen at 0.0%"),
            "{hint}"
        );
        let hint = eval_quota_probable_cause("GEMINI", Window::Weekly, &c(0.0, 300, Some(90_000)));
        assert!(hint.contains("GEMINI weekly (resets in 1d1h)"), "{hint}");

        // The threshold and freshness judgment the hint and the guard share.
        assert!(c(QUOTA_EXHAUSTED_PCT, 100, None).is_exhausted());
        assert!(!c(QUOTA_EXHAUSTED_PCT + 0.1, 100, None).is_exhausted());
        assert!(!c(0.0, QUOTA_CACHE_FRESH_S, None).is_exhausted());
        // A known refresh time replaces the 4h guess.
        assert!(c(0.0, QUOTA_CACHE_FRESH_S + 60, Some(60)).is_exhausted());
    }

    fn snap(t: u64, weekly: f64, five: f64) -> BurnSnap {
        BurnSnap {
            t,
            weekly: Some(weekly),
            five: Some(five),
        }
    }
    fn run_at(t: u64) -> BurnRun {
        BurnRun { t, ctx: 1_000 }
    }

    #[test]
    fn the_weekly_bucket_gets_its_own_runs_left() {
        // gen3dasset's week: ~1.1%/run, 1.8% left — two runs, not "5h is the limit".
        let runs: Vec<BurnRun> = [110, 120, 130, 140, 150]
            .iter()
            .map(|t| run_at(*t))
            .collect();
        let est = burn_estimate(&[snap(100, 7.3, 90.0), snap(200, 1.8, 60.0)], &runs, 300).unwrap();
        assert_eq!(weekly_runs_left(&est), Some(1));
        let out = format_burn("GEMINI", &est);
        assert!(
            out.contains("weekly -1.10%/run (~1 runs left this week)"),
            "{out}"
        );
    }

    #[test]
    fn a_single_run_is_reported_as_not_enough_instead_of_extrapolated() {
        // The exact shape that produced "~0 runs left in 5h" while 53% remained
        // (2026-08-03) and "~20 runs left" moments before a live 97% (2026-08-13):
        // two snapshots, one run between them, a big apparent drop.
        let est = burn_estimate(
            &[snap(100, 90.0, 80.0), snap(200, 89.0, 5.0)],
            &[run_at(150)],
            300,
        )
        .expect("two snapshots is enough to try");
        assert_eq!(est.runs_seen, 1);
        let out = format_burn("GEMINI", &est);
        assert!(out.contains("not enough measured runs (n=1"), "{out}");
        assert!(
            !out.contains("runs left"),
            "no projection from one sample: {out}"
        );
        assert!(!out.contains("%/run"), "no rate from one sample: {out}");
    }

    #[test]
    fn a_refilled_five_hour_window_drops_the_interval_rather_than_counting_it_as_free() {
        // The 5h window is rolling. Interval 2 refills (10 -> 60) while carrying 3
        // runs; counting those runs against zero consumption would halve the rate.
        let snaps = [
            snap(100, 90.0, 70.0),
            snap(200, 88.0, 10.0),
            snap(300, 87.0, 60.0),
        ];
        let runs: Vec<BurnRun> = [110, 120, 130, 140, 150, 210, 220, 230]
            .iter()
            .map(|t| run_at(*t))
            .collect();
        let est = burn_estimate(&snaps, &runs, 400).unwrap();
        // 5h: only interval 1 is measurable — 60 points over its 5 runs.
        assert_eq!(est.five, Some((12.0, 5)));
        // weekly never refilled, so it keeps all 8 runs (3.0 points / 8).
        assert_eq!(est.weekly, Some((3.0 / 8.0, 8)));
        assert_eq!(est.runs_seen, 8);
    }

    #[test]
    fn enough_samples_still_produce_a_rate_and_a_projection() {
        let snaps = [snap(100, 90.0, 80.0), snap(200, 85.0, 60.0)];
        let runs: Vec<BurnRun> = (1..=10).map(|i| run_at(100 + i * 5)).collect();
        let est = burn_estimate(&snaps, &runs, 300).unwrap();
        assert_eq!(est.five, Some((2.0, 10))); // 20 points / 10 runs
        let out = format_burn("GEMINI", &est);
        assert!(out.contains("5h -2.00%/run"), "{out}");
        // 60% remaining at 2%/run
        assert!(out.contains("~30 runs left in 5h"), "{out}");
    }

    #[test]
    fn snapshots_older_than_the_window_are_ignored() {
        let now = 10 * 24 * 3600;
        let stale = [snap(100, 90.0, 80.0), snap(200, 10.0, 5.0)];
        assert!(burn_estimate(&stale, &[run_at(150)], now).is_none());
    }

    #[test]
    fn test_resolve_quota_group() {
        assert_eq!(
            resolve_quota_group(Some("gemini-3.1-flash"), None),
            Some("GEMINI")
        );
        assert_eq!(
            resolve_quota_group(Some("claude-opus-4"), None),
            Some("CLAUDE AND GPT")
        );
        // No `model=` slot, but billed to the Claude bucket all the same.
        assert_eq!(
            resolve_quota_group(Some("gpt-oss-120b-medium"), None),
            Some("CLAUDE AND GPT")
        );
        assert_eq!(
            resolve_quota_group(Some("gemini-4-flash-lite-high"), None),
            Some("GEMINI")
        );
        assert_eq!(resolve_quota_group(Some("mystery-model"), None), None);
        assert_eq!(
            resolve_quota_group(None, Some("Gemini 3.5 Flash (Medium)")),
            Some("GEMINI")
        );
        assert_eq!(
            resolve_quota_group(Some("claude-opus-4"), Some("gemini-3.5-flash")),
            Some("CLAUDE AND GPT")
        );
        assert_eq!(resolve_quota_group(None, None), None);
    }

    #[test]
    fn test_steps_of() {
        assert_eq!(super::steps_of("3 steps: 3xrun_command"), 3);
        assert_eq!(super::steps_of("0 steps"), 0);
        assert_eq!(super::steps_of(""), 0);
        assert_eq!(super::steps_of("no steps"), 0);
    }

    #[test]
    fn test_quota_alternative() {
        // Gemini exhausted, Claude known at 64% -> names the newest Sonnet by name
        let quota_raw = r#"{"t":1000,"groups":{"CLAUDE AND GPT":{"5h":64.0}}}"#;
        let hint = quota_alternative("GEMINI", quota_raw, "", &cat(), 1100).unwrap();
        assert_eq!(
            hint,
            "other quota group CLAUDE AND GPT last seen 64% — retry or resume with model=claude-sonnet-5-5"
        );

        // Claude exhausted, Gemini unknown -> names the newest Flash line
        let hint_gemini = quota_alternative("CLAUDE AND GPT", "", "", &cat(), 1100).unwrap();
        assert_eq!(
            hint_gemini,
            "other quota group GEMINI — retry or resume with model=gemini-3.8-flash"
        );

        // Both exhausted -> None
        let quota_both = r#"{"t":1000,"groups":{"CLAUDE AND GPT":{"5h":0.0}}}"#;
        assert_eq!(
            quota_alternative("GEMINI", quota_both, "", &cat(), 1100),
            None
        );

        // Unknown group -> None
        assert_eq!(
            quota_alternative("UNKNOWN", quota_raw, "", &cat(), 1100),
            None
        );

        // No Sonnet in the catalog: no hint, and the refusal head says `none`.
        let no_sonnet: Vec<String> = vec![
            "gemini-3.8-flash-medium".into(),
            "claude-opus-5-5-medium".into(),
        ];
        assert_eq!(
            quota_alternative("GEMINI", quota_raw, "", &no_sonnet, 1100),
            None
        );
        let msg = quota_unavailable(WEEK_OUT, "", "GEMINI", &no_sonnet, 3000).unwrap();
        assert!(
            msg.starts_with("AGY_UNAVAILABLE: GEMINI weekly (resets in 3d10h) → none"),
            "{msg}"
        );
        // A cold cache names the alias.
        assert_eq!(
            other_quota_group("GEMINI", &[]),
            Some(("CLAUDE AND GPT", "sonnet".to_string()))
        );
    }

    const USAGE_JSON: &str = r#"{"conversation_id":"","status":"SUCCESS","response":"Gemini Models\tWeekly Limit Remaining\t47%\t2026-10-07T10:02:34Z\nGemini Models\tFive Hour Limit Remaining\t73%\t2026-10-03T10:38:22Z\nClaude and GPT models\tWeekly Limit Remaining\t98%\t2026-10-10T08:29:38Z\nClaude and GPT models\tFive Hour Limit Remaining\t96%\t2026-10-03T13:29:38Z\n","duration_seconds":0,"num_turns":0,"usage":{"input_tokens":0,"output_tokens":0,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":0},"command":{"name":"usage","data":{"description":"Within each group, models share a weekly limit and a 5-hour limit. Quota is consumed proportionally to the cost of the tokens. Thus, limits will last longer with shorter tasks or using more cost-effective models. The 5-hour limit smooths out aggregate demand to fairly distribute global capacity across all users, while your weekly limit is tied directly to your individual tier.","groups":[{"name":"Gemini Models","description":"Models within this group: Gemini Flash, Gemini Pro","buckets":[{"id":"gemini-weekly","name":"Weekly Limit Remaining","description":"You have used some of your weekly limit, it will fully refresh in 4 days.","window":"weekly","remaining_fraction":0.46629154682159424,"reset_time":"2026-10-07T10:02:34Z"},{"id":"gemini-5h","name":"Five Hour Limit Remaining","description":"You have used some of your 5-hour limit, it will fully refresh in 50 minutes.","window":"5h","remaining_fraction":0.7321233749389648,"reset_time":"2026-10-03T10:38:22Z"}]},{"name":"Claude and GPT models","description":"Models within this group: Claude Opus, Claude Sonnet, GPT-OSS","buckets":[{"id":"3p-weekly","name":"Weekly Limit Remaining","description":"You have used some of your weekly limit, it will fully refresh in 6 days, 22 hours.","window":"weekly","remaining_fraction":0.9799311757087708,"reset_time":"2026-10-10T08:29:38Z"},{"id":"3p-5h","name":"Five Hour Limit Remaining","description":"You have used some of your 5-hour limit, it will fully refresh in 3 hours, 41 minutes.","window":"5h","remaining_fraction":0.9598624110221863,"reset_time":"2026-10-03T13:29:38Z"}]}]}}}"#;

    const CREDITS_JSON: &str = r#"{"conversation_id":"","status":"SUCCESS","response":"Remaining credits\t0\nUpgrade\thttps://antigravity.google/g1-upgrade\n","duration_seconds":0,"num_turns":0,"usage":{"input_tokens":0,"output_tokens":0,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":0},"command":{"name":"credits","data":{"remaining_credits":0,"upgrade_uri":"https://antigravity.google/g1-upgrade"}}}"#;

    #[test]
    fn test_iso8601_utc_to_unix() {
        assert_eq!(iso8601_utc_to_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            iso8601_utc_to_unix("2024-02-29T12:00:00Z"),
            Some(1709208000)
        );
        assert_eq!(
            iso8601_utc_to_unix("2026-10-07T10:02:34.5Z"),
            iso8601_utc_to_unix("2026-10-07T10:02:34Z")
        );
        assert_eq!(iso8601_utc_to_unix("garbage"), None);
        assert_eq!(iso8601_utc_to_unix("2026-10-07T10:02:34"), None);
        assert_eq!(iso8601_utc_to_unix("2026-13-07T10:02:34Z"), None);
    }

    #[test]
    fn test_parse_usage_json() {
        let iso_reset = iso8601_utc_to_unix("2026-10-07T10:02:34Z").unwrap();
        let now_before = iso_reset - 1000;
        let groups = parse_usage_json(USAGE_JSON, now_before).expect("parses USAGE_JSON");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "GEMINI");
        assert_eq!(groups[1].name, "CLAUDE AND GPT");

        assert_eq!(groups[0].weekly, Some((46.63, "47%".to_string())));
        assert_eq!(groups[0].weekly_reset, Some(iso_reset - now_before));

        let now_after = iso_reset + 500;
        let groups_after = parse_usage_json(USAGE_JSON, now_after).expect("parses USAGE_JSON");
        assert_eq!(groups_after[0].weekly_reset, Some(0));

        assert!(missing_groups(&groups).is_empty());

        let entry = quota_log_entry(&groups, &[], now_before).expect("log entry");
        let gemini_entry = &entry["groups"]["GEMINI"];
        assert_eq!(gemini_entry["weekly"], 46.63);
        assert_eq!(gemini_entry["weekly_reset"], iso_reset);

        assert!(parse_usage_json("invalid json", now_before).is_none());
        assert!(parse_usage_json(r#"{"command":{"data":{}}}"#, now_before).is_none());
    }

    /// The 2026-10-16 `/usage` shape: a spent Claude weekly with a *disabled* 5h bucket
    /// (it still carries `remaining_fraction: 1`), and a weekly-only Argon group.
    const ARGON_USAGE_JSON: &str = r#"{"command":{"data":{"groups":[
        {"name":"Gemini Models","buckets":[
            {"window":"weekly","remaining_fraction":1,"reset_time":"2026-10-23T02:55:59Z"},
            {"window":"5h","remaining_fraction":1,"reset_time":"2026-10-16T07:55:59Z"}]},
        {"name":"Claude and GPT models","buckets":[
            {"window":"weekly","remaining_fraction":0,"reset_time":"2026-10-19T15:19:05Z"},
            {"window":"5h","remaining_fraction":1,"disabled":true}]},
        {"name":"Gemini 4 Argon","buckets":[
            {"window":"weekly","remaining_fraction":1,"reset_time":"2026-10-23T02:55:59Z"}]}]}}}"#;

    #[test]
    fn argon_has_its_own_weekly_only_group() {
        let now = iso8601_utc_to_unix("2026-10-16T05:00:00Z").unwrap();
        let groups = parse_usage_json(ARGON_USAGE_JSON, now).expect("parses");
        assert_eq!(groups[2].name, ARGON_GROUP);
        assert!(groups[2].weekly.is_some() && groups[2].five.is_none());
        // A disabled bucket is shown as such and never logged as a 0%/100% reading.
        assert_eq!(
            groups[1].five.as_ref().map(|f| f.1.as_str()),
            Some("disabled")
        );
        let entry = quota_log_entry(&groups, &[], now).expect("logged");
        assert!(entry["groups"]["CLAUDE AND GPT"].get("5h").is_none());
        assert!(entry["groups"][ARGON_GROUP].get("weekly").is_some());
        let disp = format_usage(&groups, None).unwrap();
        assert!(disp.contains("5h disabled"), "{disp}");
        assert!(disp.contains(ARGON_GROUP), "{disp}");
    }

    #[test]
    fn an_argon_run_bills_to_the_argon_group_not_gemini() {
        for l in ["gemini-4-argon-high", "Gemini 4 Argon (Medium)"] {
            assert_eq!(quota_group_of(l), Some(ARGON_GROUP), "{l}");
        }
        assert_eq!(quota_group_of("gemini-3.8-flash-high"), Some("GEMINI"));
        // The run log's `served` receipt feeds the argon group's own recency check.
        let runs = r#"{"ok":true,"t":200,"served":"Gemini 4 Argon (Medium)","model":"gemini-4-argon-medium"}"#;
        assert!(ok_run_since(runs, ARGON_GROUP, 100));
        assert!(!ok_run_since(runs, "GEMINI", 100));
        // An exhausted argon bucket points at the independent Gemini one.
        assert_eq!(
            other_quota_group(ARGON_GROUP, &cat()),
            Some(("GEMINI", "gemini-3.8-flash".to_string()))
        );
    }

    #[test]
    fn a_usage_group_the_bridge_has_no_name_for_is_said_out_loud() {
        let json = r#"{"command":{"data":{"groups":[
            {"name":"Gemini Models","buckets":[{"window":"weekly","remaining_fraction":0.5}]},
            {"name":"Gemini 5 Xenon","buckets":[{"window":"weekly","remaining_fraction":0.9}]}]}}}"#;
        let groups = parse_usage_json(json, 0).unwrap();
        assert_eq!(unguarded_groups(&groups), vec!["GEMINI 5 XENON"]);
        assert!(unguarded_group_note("GEMINI 5 XENON").contains("not guarded, counted"));
        // The three known groups are never reported.
        let known = parse_usage_json(ARGON_USAGE_JSON, 0).unwrap();
        assert!(unguarded_groups(&known).is_empty());
    }

    #[test]
    fn a_plan_without_the_argon_row_is_not_reported_missing_or_printed_as_unknown() {
        let no_argon = r#"{"command":{"data":{"groups":[
            {"name":"Gemini Models","buckets":[{"window":"weekly","remaining_fraction":0.5}]},
            {"name":"Claude and GPT models","buckets":[{"window":"weekly","remaining_fraction":0.5}]}]}}}"#;
        let groups = parse_usage_json(no_argon, 0).unwrap();
        assert!(missing_groups(&groups).is_empty());
        let log = r#"{"t":1,"groups":{"GEMINI":{"weekly":50.0},"CLAUDE AND GPT":{"weekly":50.0}}}"#;
        let line = status_line(log, "", 100).unwrap();
        assert!(!line.contains("ARGON"), "{line}");
        let with =
            r#"{"t":1,"groups":{"GEMINI":{"weekly":50.0},"GEMINI 4 ARGON":{"weekly":80.0}}}"#;
        assert!(status_line(with, "", 100)
            .unwrap()
            .contains("GEMINI 4 ARGON weekly 80%"));
    }

    #[test]
    fn test_parse_credits_json() {
        assert_eq!(parse_credits_json(CREDITS_JSON), Some(0));
    }
}
