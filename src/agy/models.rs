use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::process::Stdio;

use crate::agy::paths::{agy_bin, agy_mcp_home};
use crate::agy::quota::now_unix;

fn models_cache_path() -> PathBuf {
    agy_mcp_home().join("models.json")
}

use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default)]
struct ModelsCache {
    fetched_at: u64,
    models: Vec<String>,
}

/// Cached model slugs from the last refresh (empty if never fetched).
/// Refreshed automatically — see [`refresh_models_cache`].
pub fn load_models_cache() -> Vec<String> {
    read_models_cache().map(|c| c.models).unwrap_or_default()
}

fn read_models_cache() -> Option<ModelsCache> {
    serde_json::from_str::<ModelsCache>(&std::fs::read_to_string(models_cache_path()).ok()?).ok()
}

pub(crate) fn save_models_cache(models: &[String]) {
    let _ = std::fs::create_dir_all(agy_mcp_home());
    let c = ModelsCache {
        fetched_at: now_unix(),
        models: models.to_vec(),
    };
    let Ok(s) = serde_json::to_string_pretty(&c) else {
        return;
    };
    let path = models_cache_path();
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    // Atomic: instances coexist and several may refresh at once — a torn file
    // reads back as an empty cache, which silently falls through to the consts.
    if std::fs::write(&tmp, s).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// How old a model cache may get before a startup refresh is worth the ~2s.
pub(crate) const MODELS_CACHE_TTL_S: u64 = 24 * 3600;

/// Pure staleness test, split out for unit tests. Stale = empty, older than the
/// TTL, or holding **legacy human labels** (`"Gemini 3.5 Flash (Low)"`) instead
/// of the `agy models` slugs the resolvers now expect — that last case is what
/// used to require a manual "run agy_diag(live, what=models) once after
/// deploying" step.
pub fn models_cache_is_stale(cache: Option<(u64, &[String])>, now: u64) -> bool {
    let Some((fetched_at, models)) = cache else {
        return true;
    };
    models.is_empty()
        || now.saturating_sub(fetched_at) > MODELS_CACHE_TTL_S
        || models.iter().any(|m| m.contains(' ') || m.contains('('))
}

/// True when the on-disk cache should be refreshed before it is trusted.
pub fn models_cache_needs_refresh() -> bool {
    let c = read_models_cache();
    models_cache_is_stale(
        c.as_ref().map(|c| (c.fetched_at, c.models.as_slice())),
        now_unix(),
    )
}

/// Why agy cannot serve at all, from the last `agy models` call; None = it
/// answered, or it hasn't been asked yet.
static AGY_DOWN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The last `agy models` failure, if the last call failed. Behind `AGY_DOWN:`.
pub fn agy_down() -> Option<String> {
    AGY_DOWN.lock().map(|g| g.clone()).unwrap_or(None)
}

/// A hung `agy models` is not evidence of anything; past this the probe gives up
/// and leaves the health verdict as it was.
const MODELS_PROBE_TIMEOUT_S: u64 = 15;

/// Refresh the model cache from the headless `agy models` subcommand. Zero
/// quota, no TUI, ~2s. Returns true if the cache changed, so a caller that just
/// failed to resolve a slot knows whether a retry has any chance.
///
/// Doubles as the health probe: a missing or broken exe (e.g. a failed upstream
/// update) fails this call, and the failure is kept for [`agy_down`].
pub async fn refresh_models_cache() -> bool {
    let before = load_models_cache();
    let probe = tokio::time::timeout(
        std::time::Duration::from_secs(MODELS_PROBE_TIMEOUT_S),
        tokio::task::spawn_blocking(list_model_slugs),
    )
    .await;
    let Ok(res) = probe else {
        return false;
    };
    let down = match &res {
        Ok(Ok(_)) => None,
        Ok(Err(e)) => Some(format!("{e:#}")),
        Err(e) => Some(format!("`agy models` probe panicked: {e}")),
    };
    if let Ok(mut g) = AGY_DOWN.lock() {
        *g = down;
    }
    match res {
        Ok(Ok(slugs)) if !slugs.is_empty() => {
            save_models_cache(&slugs);
            slugs != before
        }
        _ => false,
    }
}

/// Guard the served-model receipt against false positives. The receipt is found
/// by a literal substring search, so ANY transcript content quoting that phrase
/// matches — including this very source file when the sub-agent is asked to read
/// or edit `agy.rs` (observed: a run recorded `") else { continue };\n773: …` as
/// its served model). A real label is one short line like "Gemini 3.5 Flash
/// (High)", so reject multi-line or overlong captures.
pub(crate) fn is_plausible_model_label(label: &str) -> bool {
    !label.is_empty()
        && label != "None"
        && label.len() <= 60
        && !label.contains('\n')
        && !label.contains('\r')
}

/// One of the three model "slots" agy abstracts over. Versions change
/// server-side; we resolve the current label per slot from the cache.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Slot {
    Flash,
    Pro,
    Claude,
}

/// Codenames agy gives Gemini lines that carry neither `flash` nor `pro` in the slug,
/// and the slot each one stands for. The slot is the *role* (the nominal line); the
/// codename is just today's model in it. `argon` (Gemini 4, agy 1.3.x, 2026-10) is the
/// successor to the Pro line. Adding the next codename is one row here plus a test —
/// until then it is listed without an alias tag and reached only by name.
const CODENAME_SLOTS: &[(&str, Slot)] = &[("argon", Slot::Pro)];

/// Classify a model label into its slot (None = no `model=` slot: GPT-OSS, a lite
/// tier, or a name the classifier doesn't know yet — still listed and still
/// reachable by name, just never picked by the `flash|pro|…` aliases).
/// Case-insensitive and accepts BOTH the human label ("Gemini 3.6 Flash (High)",
/// as written in the transcript receipt) and the stable machine slug
/// ("gemini-3.6-flash-high", as listed by `agy models` / passed to `--model`).
pub fn slot_of(label: &str) -> Option<Slot> {
    let l = label.to_ascii_lowercase();
    if l.contains("gpt-oss") || l.contains("gpt_oss") {
        None
    } else if l.contains("claude") {
        Some(Slot::Claude)
    } else if l.contains("gemini") && l.contains("pro") {
        Some(Slot::Pro)
    } else if let Some((_, slot)) = CODENAME_SLOTS
        .iter()
        .find(|(name, _)| l.contains("gemini") && l.contains(name))
    {
        Some(*slot)
    } else if l.contains("gemini") && l.contains("flash") && !l.contains("lite") {
        // A Flash-Lite tier is a different model, not a cheaper effort of Flash:
        // letting it into the Flash slot would let `model=flash` pick it whenever its
        // version number is the newest.
        Some(Slot::Flash)
    } else {
        None
    }
}

/// True if `requested` and `served` resolve to different slots (a silent
/// downgrade/fallback). When either side has no slot (GPT-OSS, an unknown codename),
/// their [`family_key`]s are compared instead — but only when both start with a
/// vendor prefix (`gemini|claude|gpt`): an unclassifiable label must never trigger a
/// false warning.
pub fn slot_mismatch(requested: &str, served: Option<&str>) -> bool {
    let Some(served) = served else { return false };
    match (slot_of(requested), slot_of(served)) {
        (Some(want), Some(have)) => want != have,
        _ => {
            let (want, have) = (family_key(requested), family_key(served));
            let known = |f: &str| ["gemini", "claude", "gpt"].iter().any(|p| f.starts_with(p));
            known(&want) && known(&have) && want != have
        }
    }
}

/// Extract a model version `(major, minor)` from a label or slug: the first
/// version token, in any of the forms agy has used —
/// dotted (`gemini-3.6-flash-high`, "Gemini 3.6 Flash"), dash-split
/// (`claude-sonnet-5-5-high` → (5,5)), or a bare major (`gemini-4-flash-high` →
/// (4,0)). The bare-major form is load-bearing for the next generation: reading it
/// as None would rank `gemini-4-*` *below* 3.8 and keep `model=flash` on the old
/// line. A digit run glued to letters (`120b`) is not a version. None if no token.
pub fn model_version(label: &str) -> Option<(u32, u32)> {
    let toks: Vec<&str> = label
        .split(|c: char| c == '-' || c == '(' || c == ')' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .collect();
    for (i, tok) in toks.iter().enumerate() {
        if let Some((a, b)) = tok.split_once('.') {
            if let (Ok(x), Ok(y)) = (a.parse::<u32>(), b.parse::<u32>()) {
                return Some((x, y));
            }
        } else if let Ok(x) = tok.parse::<u32>() {
            let y = toks.get(i + 1).and_then(|n| n.parse::<u32>().ok());
            return Some((x, y.unwrap_or(0)));
        }
    }
    None
}

/// True when agy refused the `--model` value itself (`invalid model selection …
/// is not recognized`). agy does that before any model call — zero quota — and it
/// means the cache still holds a slug the catalog has dropped.
pub fn is_invalid_model_selection(err_text: &str) -> bool {
    err_text.contains("invalid model selection")
}

/// Strip a trailing effort suffix (`-high|-medium|-low`) from a slug, collapsing
/// effort variants to one base line (Gemini, and Claude since 5.5). Slugs without
/// such a suffix are returned unchanged.
pub fn base_slug(slug: &str) -> String {
    for suf in ["-high", "-medium", "-low"] {
        if let Some(b) = slug.strip_suffix(suf) {
            return b.to_string();
        }
    }
    slug.to_string()
}

/// Normalize a slug or a display label to one model-family name: lowercase, drop a
/// trailing effort (`-high|-medium|-low` or `(High)`…), and collapse every run of
/// `.`, `-`, whitespace and parentheses to a single `-`. `gemini-4-argon-medium` and
/// "Gemini 4 Argon (Medium)" both become `gemini-4-argon`; `claude-sonnet-5-5-high`
/// and "Claude Sonnet 5.5 (High)" both `claude-sonnet-5-5`. A parenthesized word that
/// is not an effort (`(Thinking)`) is part of the name and stays. Pure.
pub fn family_key(label: &str) -> String {
    let lower = label.to_ascii_lowercase();
    let mut toks: Vec<&str> = lower
        .split(|c: char| c == '.' || c == '-' || c == '(' || c == ')' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .collect();
    if toks.len() > 1 && matches!(toks.last(), Some(&("low" | "medium" | "high"))) {
        toks.pop();
    }
    toks.join("-")
}

/// Headless model list via the `agy models` subcommand (stable machine slugs,
/// e.g. "gemini-3.6-flash-high"). Zero quota, no TUI scrape — the robust primary
/// source that replaces the ConPTY `/model` scrape for model discovery.
pub fn list_model_slugs() -> anyhow::Result<Vec<String>> {
    let bin = agy_bin();
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("models")
        .stdin(Stdio::null())
        .envs(crate::nest::spawned_env());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Invariant #1: the MCP server is windowless, so every shelled-out child needs
        // this or a console flashes on screen. Load-bearing now that this runs
        // unattended (startup refresh + resolve-miss retry), not just on agy_diag(live).
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd
        .output()
        .with_context(|| format!("running `{bin} models`"))?;
    if !out.status.success() {
        anyhow::bail!("`agy models` exited {:?}", out.status.code());
    }
    let slugs: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(slug_from_models_line)
        .collect();
    Ok(slugs)
}

/// One line of `agy models` → its machine slug, or None if the line isn't one.
///
/// agy prints `<slug>\t<display label>` (the label column was there on 2026-08-13;
/// older builds printed the slug alone). Taking the field before the first TAB reads
/// both. **Load-bearing:** without the split, the whole tab-joined line was cached and
/// handed to `--model` verbatim, and agy rejected it — `invalid model selection
/// (--model "gemini-3.7-flash-low\tGemini 3.7 Flash (Low)")`, killing every run that
/// named a model. The cache's own staleness check flagged it (it contains a space) but
/// could not repair it: the refresh re-listed through this function and wrote the same
/// broken form back, so the cache was perpetually stale and perpetually wrong.
///
/// A slug never contains whitespace, so a line whose first field does is refused rather
/// than cached — an unparsable list reads as an empty cache, which falls back to the
/// consts instead of shipping a string agy will reject.
///
/// **Every model agy lists is kept**, slot or not (GPT-OSS, a lite tier, a codename
/// the classifier has never seen). They used to be dropped here, which hid them from
/// the model list and made `model_label` reject them. A slot-less row is accepted on
/// the strength of agy's own label column; a bare slug-only line still needs a slot.
pub(crate) fn slug_from_models_line(line: &str) -> Option<String> {
    let mut cols = line.split('\t');
    let slug = cols.next()?.trim();
    let labelled = cols.next().is_some_and(|l| !l.trim().is_empty());
    let well_formed = !slug.is_empty() && !slug.contains(char::is_whitespace);
    (well_formed && (labelled || slot_of(slug).is_some())).then(|| slug.to_string())
}

/// Live model list from the headless `agy models` subcommand (stable machine
/// slugs, zero quota). Updates the cache and returns a concise, slot-grouped
/// summary plus the raw slugs.
pub async fn live_models() -> (String, Vec<String>) {
    match tokio::task::spawn_blocking(list_model_slugs).await {
        Ok(Ok(slugs)) if !slugs.is_empty() => {
            save_models_cache(&slugs);
            (format_models("agy models", &slugs, None), slugs)
        }
        _ => (
            "Could not list models (`agy models` failed).".into(),
            vec![],
        ),
    }
}

/// One line per model family for display: `<base> : <efforts> [(newest)] [<alias>]`.
/// Effort variants (`-low|-medium|-high`) collapse onto their family; `(newest)` marks
/// the newest version of a line that has more than one; `[flash]` etc. marks the
/// family each `model=` alias resolves to today (newest version in the slot, Sonnet
/// and Opus apart). Every model is listed, slot or not — all are reachable by name.
pub(crate) fn format_models(source: &str, labels: &[String], current: Option<&str>) -> String {
    struct Fam {
        base: String,
        key: String,
        efforts: Vec<&'static str>,
        slot: Option<Slot>,
        ver: Option<(u32, u32)>,
    }
    let mut fams: Vec<Fam> = Vec::new();
    for l in labels {
        let key = family_key(l);
        let i = match fams.iter().position(|f| f.key == key) {
            Some(i) => i,
            None => {
                fams.push(Fam {
                    base: base_slug(l),
                    key,
                    efforts: vec![],
                    slot: slot_of(l),
                    ver: model_version(l),
                });
                fams.len() - 1
            }
        };
        if let Some(e) = ["low", "medium", "high"]
            .into_iter()
            .find(|e| l.ends_with(&format!("-{e}")))
        {
            fams[i].efforts.push(e);
        }
    }
    let rank = |e: &str| ["low", "medium", "high"].iter().position(|x| *x == e);
    for f in &mut fams {
        f.efforts.sort_by_key(|e| rank(e));
        f.efforts.dedup();
    }
    // A line is a family without its version numbers (`gemini-flash`, `claude-sonnet`).
    let line = |key: &str| -> String {
        key.split('-')
            .filter(|t| !t.chars().all(|c| c.is_ascii_digit()))
            .collect::<Vec<_>>()
            .join("-")
    };
    // The aliases, each with the families it may pick from (mirrors the resolvers).
    let aliases: [(&str, fn(&Fam) -> bool); 4] = [
        ("flash", |f| f.slot == Some(Slot::Flash)),
        ("pro", |f| f.slot == Some(Slot::Pro)),
        ("sonnet", |f| {
            f.slot == Some(Slot::Claude) && f.key.contains("sonnet")
        }),
        ("opus", |f| {
            f.slot == Some(Slot::Claude) && f.key.contains("opus")
        }),
    ];
    let tags = |f: &Fam| -> Vec<&str> {
        aliases
            .iter()
            .filter(|(_, pick)| {
                pick(f) && fams.iter().filter(|g| pick(g)).map(|g| g.ver).max() == Some(f.ver)
            })
            .map(|(name, _)| *name)
            .collect()
    };
    let slot_rank = |s: Option<Slot>| match s {
        Some(Slot::Flash) => 0,
        Some(Slot::Pro) => 1,
        Some(Slot::Claude) => 2,
        None => 3,
    };
    let mut order: Vec<&Fam> = fams.iter().collect();
    order.sort_by(|a, b| {
        (slot_rank(a.slot), line(&a.key), std::cmp::Reverse(a.ver)).cmp(&(
            slot_rank(b.slot),
            line(&b.key),
            std::cmp::Reverse(b.ver),
        ))
    });

    let mut out = vec![format!("agy models (source: {source})")];
    if order.is_empty() {
        out.push("  (none)".to_string());
    }
    for f in &order {
        let same_line: Vec<&&Fam> = order
            .iter()
            .filter(|g| line(&g.key) == line(&f.key))
            .collect();
        let newest = same_line.len() > 1 && same_line.iter().map(|g| g.ver).max() == Some(f.ver);
        let efforts = if f.efforts.is_empty() {
            "—".to_string()
        } else {
            f.efforts.join("·")
        };
        let t = tags(f);
        out.push(format!(
            "  {} : {efforts}{}{}",
            f.base,
            if newest { " (newest)" } else { "" },
            if t.is_empty() {
                String::new()
            } else {
                format!(" [{}]", t.join(","))
            }
        ));
    }
    if let Some(c) = current {
        out.push(format!("  current: {c}"));
    }
    out.push(
        "Pick: model=<alias> (the [ ] rows, always the newest) or a name (left column, or a \
         word such as argon; a -low/-high suffix pins the effort)"
            .to_string(),
    );
    out.push("Cache updated: ~/.agy-mcp/models.json (agy_sub reads it)".to_string());
    out.join("\n")
}

/// Quota (`/usage`) + models (`agy models`), both headless.
pub async fn live_both() -> String {
    let q = crate::agy::quota::live_quota().await;
    let (m, _) = live_models().await;
    format!("{q}\n\n{m}")
}

/// Convert a Gemini catalog slug (`gemini-<maj>.<min>-<flash|pro>[-<low|medium|high>]`)
/// into agy's display label expected on `--model`: `Gemini <maj>.<min> Flash (High)`,
/// `Gemini 3.1 Pro (Low)`, or `Gemini 3.6 Flash` (no parentheses if no effort).
///
/// WHY THIS EXISTS: agy 1.1.5's print mode does not honor certain catalog slugs.
/// Specifically, `gemini-3.1-pro-high` is served as Gemini 3.6 Flash (family downgrade)
/// and `gemini-3.5-flash-low` is served at Medium (effort drift). Passing agy's
/// display label instead is exact. Upstream issue: google-antigravity/antigravity-cli#581.
/// Reverting this workaround = making this function the identity function (`m.to_string()`).
///
/// Returns non-Gemini slugs (`claude-*`, `gpt-oss-*`), already-formatted labels,
/// and unrecognized strings unchanged. Pure & idempotent.
pub fn wire_model(m: &str) -> String {
    let s = m.trim();
    if !s.starts_with("gemini-") {
        return m.to_string();
    }

    // Expect: "gemini", "<maj>.<min>", "<flash|pro>", optional "<low|medium|high>"
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() < 3 || parts.len() > 4 {
        return m.to_string();
    }

    let version = parts[1];
    let family_raw = parts[2];
    let family = match family_raw {
        "flash" => "Flash",
        "pro" => "Pro",
        _ => return m.to_string(),
    };

    let effort = if parts.len() == 4 {
        match parts[3] {
            "low" => Some("Low"),
            "medium" => Some("Medium"),
            "high" => Some("High"),
            _ => return m.to_string(),
        }
    } else {
        None
    };

    match effort {
        Some(eff) => format!("Gemini {version} {family} ({eff})"),
        None => format!("Gemini {version} {family}"),
    }
}

/// Validate a `model_label` against the cached model slugs.
///
/// An EMPTY cache accepts anything, mirroring invariant #4: the consts are consulted
/// only when the cache is empty, because a rotation produces a DIFFERENT list rather
/// than an absent one. Rejecting a correct slug while the cache is cold would be the
/// bridge inventing an outage.
///
/// A populated cache without the slug is a rejection, and the message names the
/// siblings in the same base family — the measured mistake is a dropped effort suffix
/// (`gemini-3.7-flash`), whose answer is the three forms that do exist. The caller is
/// deliberately NOT told to refresh anything: the call site re-lists once first, so by
/// the time this message is seen the list is already fresh.
pub fn validate_model_label(label: &str, cache_slugs: &[String]) -> Result<(), String> {
    if cache_slugs.is_empty() || cache_slugs.iter().any(|s| s == label) {
        return Ok(());
    }

    let target_base = base_slug(label);
    let siblings: Vec<&str> = cache_slugs
        .iter()
        .filter(|s| base_slug(s) == target_base && s.as_str() != label)
        .map(|s| s.as_str())
        .collect();

    let mut msg = format!("model '{label}' is not in agy's model catalog.");
    if !siblings.is_empty() {
        msg.push_str(&format!(
            " Available siblings in family '{target_base}': {}.",
            siblings.join(", ")
        ));
    }
    Err(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base_slug() {
        assert_eq!(base_slug("gemini-3.6-flash-high"), "gemini-3.6-flash");
        assert_eq!(base_slug("gemini-3.1-pro-low"), "gemini-3.1-pro");
        assert_eq!(
            base_slug("claude-opus-4-6-thinking"),
            "claude-opus-4-6-thinking"
        );
        assert_eq!(base_slug("claude-sonnet-5-5-high"), "claude-sonnet-5-5");
    }

    #[test]
    fn family_key_joins_slugs_and_labels() {
        assert_eq!(family_key("gemini-4-argon-medium"), "gemini-4-argon");
        assert_eq!(family_key("Gemini 4 Argon (Medium)"), "gemini-4-argon");
        assert_eq!(family_key("claude-sonnet-5-5-high"), "claude-sonnet-5-5");
        assert_eq!(family_key("Claude Sonnet 5.5 (High)"), "claude-sonnet-5-5");
        assert_eq!(family_key("gpt-oss-120b-medium"), "gpt-oss-120b");
        assert_eq!(family_key("GPT-OSS 120B (Medium)"), "gpt-oss-120b");
        assert_eq!(family_key("gemini-4-argon"), "gemini-4-argon");
        assert_eq!(family_key("gemini-3.8-flash-low"), "gemini-3-8-flash");
        // `(Thinking)` is not an effort: the old Claude labels keep their own family.
        assert_eq!(
            family_key("Claude Opus 4.6 (Thinking)"),
            "claude-opus-4-6-thinking"
        );
        assert_eq!(
            family_key("claude-opus-4-6-thinking"),
            "claude-opus-4-6-thinking"
        );
    }

    #[test]
    fn format_models_lists_one_line_per_family_with_alias_tags() {
        let labels: Vec<String> = [
            "gemini-4-argon-medium",
            "gemini-4-argon-high",
            "gemini-4-argon-low",
            "gemini-3.8-flash-medium",
            "claude-opus-5-5-low",
            "claude-opus-5-5-high",
            "claude-sonnet-5-5-low",
            "claude-sonnet-5-5-high",
            "gpt-oss-120b-medium",
        ]
        .map(String::from)
        .to_vec();
        let res = format_models("test", &labels, None);
        let line = |name: &str| res.lines().find(|l| l.contains(name)).unwrap().to_string();
        assert_eq!(line("argon"), "  gemini-4-argon : low·medium·high [pro]");
        assert_eq!(line("flash"), "  gemini-3.8-flash : medium [flash]");
        assert_eq!(line("claude-opus"), "  claude-opus-5-5 : low·high [opus]");
        assert_eq!(
            line("claude-sonnet"),
            "  claude-sonnet-5-5 : low·high [sonnet]"
        );
        // A slot-less model is listed like any other, just without a tag.
        assert_eq!(line("gpt-oss"), "  gpt-oss-120b : medium");
        // Slot order: flash, pro, claude, then the rest.
        let pos = |n: &str| res.find(n).unwrap();
        assert!(pos("gemini-3.8-flash") < pos("gemini-4-argon"));
        assert!(pos("claude-sonnet") < pos("gpt-oss"));
    }

    #[test]
    fn invalid_model_selection_is_recognized() {
        assert!(is_invalid_model_selection(
            "Error: invalid model selection (--model \"claude-sonnet-4-6\" --effort \"\"): \
             model claude-sonnet-4-6 is not recognized"
        ));
        assert!(!is_invalid_model_selection("429 RESOURCE_EXHAUSTED"));
    }

    #[test]
    fn test_format_models_gemini_effort_collapsing() {
        let labels = vec![
            "gemini-3.6-flash-high".to_string(),
            "gemini-3.6-flash-low".to_string(),
            "gemini-3.5-flash-high".to_string(),
        ];
        let res = format_models("test", &labels, None);
        // Two versions of one line: the newest is marked and carries the alias.
        assert!(
            res.contains("  gemini-3.6-flash : low·high (newest) [flash]"),
            "{res}"
        );
        assert!(
            res.contains(
                "  gemini-3.5-flash : high
"
            ),
            "{res}"
        );
        assert!(res.find("gemini-3.6-flash").unwrap() < res.find("gemini-3.5-flash").unwrap());
    }

    #[test]
    fn slot_of_handles_slugs_and_labels() {
        // Stable machine slugs (agy 1.1.5+).
        assert_eq!(slot_of("gemini-3.6-flash-high"), Some(Slot::Flash));
        assert_eq!(slot_of("gemini-3.1-pro-low"), Some(Slot::Pro));
        assert_eq!(slot_of("claude-sonnet-4-6"), Some(Slot::Claude));
        assert_eq!(slot_of("claude-opus-4-6-thinking"), Some(Slot::Claude));
        assert_eq!(slot_of("gpt-oss-120b-medium"), None);
        assert_eq!(slot_of("claude-opus-5-5-high"), Some(Slot::Claude));
        // A lite tier is its own model, never the Flash slot's pick.
        assert_eq!(slot_of("gemini-4-flash-lite-medium"), None);
        assert_eq!(slot_of("gemini-4-flash-high"), Some(Slot::Flash));
        // Argon (agy 1.3.x, 2026-10) is the Pro line by codename; its quota group is
        // routed separately by `quota_group_of`. A codename not in the table has no slot.
        assert_eq!(slot_of("gemini-4-argon-high"), Some(Slot::Pro));
        assert_eq!(slot_of("Gemini 4 Argon (Medium)"), Some(Slot::Pro));
        assert_eq!(slot_of("gemini-4-xenon-high"), None);
        assert_eq!(model_version("gemini-4-argon-high"), Some((4, 0)));
        // Legacy human labels (still used in the transcript receipt).
        assert_eq!(slot_of("Gemini 3.5 Flash (High)"), Some(Slot::Flash));
        assert_eq!(slot_of("Gemini 3.1 Pro (High)"), Some(Slot::Pro));
        assert_eq!(slot_of("Claude Sonnet 4.6 (Thinking)"), Some(Slot::Claude));
    }

    #[test]
    fn test_slot_mismatch() {
        assert!(!slot_mismatch(
            "gemini-3.1-pro-low",
            Some("gemini-3.1-pro-medium")
        ));
        assert!(slot_mismatch(
            "gemini-3.1-pro-low",
            Some("gemini-3.6-flash-high")
        ));
        assert!(!slot_mismatch("gemini-3.1-pro-low", None));
        assert!(!slot_mismatch(
            "some-unknown-label",
            Some("gemini-3.6-flash-high")
        ));
    }

    #[test]
    fn slot_mismatch_compares_families_when_a_side_has_no_slot() {
        // Same family, slug vs receipt label: a match.
        assert!(!slot_mismatch(
            "gpt-oss-120b-medium",
            Some("GPT-OSS 120B (Medium)")
        ));
        assert!(!slot_mismatch(
            "gemini-4-xenon-high",
            Some("Gemini 4 Xenon (Low)")
        ));
        // A slot-less request served by another family is a swap, either way round.
        assert!(slot_mismatch(
            "gpt-oss-120b-medium",
            Some("Gemini 3.8 Flash (Medium)")
        ));
        assert!(slot_mismatch(
            "gemini-3.8-flash-medium",
            Some("GPT-OSS 120B (Medium)")
        ));
        // An unknown prefix stays silent, and so does an empty receipt.
        assert!(!slot_mismatch(
            "mystery-model-1",
            Some("GPT-OSS 120B (Medium)")
        ));
        assert!(!slot_mismatch("gpt-oss-120b-medium", Some("")));
    }

    #[test]
    fn test_requested_mismatch_stats() {
        let entries = vec![
            serde_json::json!({"model": "gemini-3.1-pro-low", "served": "gemini-3.1-pro-low"}),
            serde_json::json!({"model": "gemini-3.1-pro-low", "served": "gemini-3.6-flash-high"}),
            serde_json::json!({"model": "gemini-3.1-pro-low", "served": ""}),
            serde_json::json!({"model": "gemini-3.6-flash-low", "served": "gemini-3.6-flash-high"}),
            serde_json::json!({"model": "default", "served": "gemini-3.6-flash-high"}),
            serde_json::json!({"model": "", "served": "gemini-3.6-flash-high"}),
        ];
        let stats = crate::agy::quota::requested_mismatch_stats(&entries);
        assert_eq!(stats.len(), 2);
        // 2, not 3: the `served: ""` row is a run that failed before agy reported a
        // model, so it is evidence of neither a match nor a mismatch. This assertion
        // said 3 until 2026-08-14 — `slot_mismatch` rightly refuses to judge an
        // unclassifiable side, which quietly made every such row count as a match.
        assert_eq!(stats.get("gemini-3.1-pro-low"), Some(&(1, 2)));
        assert_eq!(stats.get("gemini-3.6-flash-low"), Some(&(0, 1)));
    }

    #[test]
    fn model_version_extracts_every_form() {
        assert_eq!(model_version("gemini-3.6-flash-high"), Some((3, 6)));
        assert_eq!(model_version("Gemini 3.5 Flash (High)"), Some((3, 5)));
        assert_eq!(model_version("gemini-3.1-pro-low"), Some((3, 1)));
        assert_eq!(model_version("3.5"), Some((3, 5)));
        // Dash-split Claude versions.
        assert_eq!(model_version("claude-sonnet-4-6"), Some((4, 6)));
        assert_eq!(model_version("claude-opus-5-5-high"), Some((5, 5)));
        assert_eq!(model_version("Claude Opus 5.5 (High)"), Some((5, 5)));
        // A bare major outranks the old dotted line.
        assert_eq!(model_version("gemini-4-flash-high"), Some((4, 0)));
        assert_eq!(model_version("Gemini 4 Flash (High)"), Some((4, 0)));
        assert!(model_version("gemini-4-flash-high") > model_version("gemini-3.8-flash-high"));
        // A digit run glued to letters is a size, not a version.
        assert_eq!(model_version("gpt-oss-120b-medium"), None);
        assert_eq!(model_version("no-version-here"), None);
    }

    #[test]
    fn served_label_guard_rejects_code_snippets() {
        assert!(is_plausible_model_label("Gemini 3.5 Flash (High)"));
        assert!(is_plausible_model_label("Claude Sonnet 4.6 (Thinking)"));
        assert!(!is_plausible_model_label(""));
        assert!(!is_plausible_model_label("None"));
        // the real-world false positive: agy.rs quoted back at us
        assert!(!is_plausible_model_label(
            "\") else { continue };\n773:  let after ="
        ));
        assert!(!is_plausible_model_label(&"x".repeat(61)));
    }

    #[test]
    fn slug_from_models_line_keeps_only_the_slug_column() {
        // The two-column form agy prints (2026-08-13). Caching the whole line sent
        // `--model "gemini-3.7-flash-low\tGemini 3.7 Flash (Low)"` and agy rejected it.
        assert_eq!(
            slug_from_models_line("gemini-3.7-flash-low\tGemini 3.7 Flash (Low)").as_deref(),
            Some("gemini-3.7-flash-low")
        );
        // Older slug-only form still reads.
        assert_eq!(
            slug_from_models_line("claude-sonnet-4-6").as_deref(),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            slug_from_models_line("  gemini-3.1-pro-high\t Gemini 3.1 Pro (High) ").as_deref(),
            Some("gemini-3.1-pro-high")
        );
        // Non-model chatter is refused.
        assert_eq!(slug_from_models_line("Fetching available models..."), None);
        assert_eq!(slug_from_models_line(""), None);
        // Slot-less models agy lists are kept (they used to be hidden here).
        assert_eq!(
            slug_from_models_line("gpt-oss-120b-medium\tGPT-OSS 120B (Medium)").as_deref(),
            Some("gpt-oss-120b-medium")
        );
        assert_eq!(
            slug_from_models_line("gemini-argon-preview\tGemini Argon (Preview)").as_deref(),
            Some("gemini-argon-preview")
        );
        // …but a bare slug-only line still needs a slot to be believed.
        assert_eq!(slug_from_models_line("gpt-oss-120b-medium"), None);
        // A whitespace-bearing first field is not a slug: refuse it rather than cache
        // a string `--model` would reject.
        assert_eq!(slug_from_models_line("Gemini 3.7 Flash (Low)"), None);
    }

    #[test]
    fn test_models_cache_is_stale() {
        let now = 1_000_000u64;
        let slugs = [
            "gemini-3.6-flash-low".to_string(),
            "claude-sonnet-4-6".to_string(),
        ];
        // Fresh slug cache — leave it alone.
        assert!(!models_cache_is_stale(Some((now - 60, &slugs)), now));
        // Never fetched / empty / past the TTL.
        assert!(models_cache_is_stale(None, now));
        assert!(models_cache_is_stale(Some((now, &[])), now));
        assert!(models_cache_is_stale(
            Some((now - MODELS_CACHE_TTL_S - 1, &slugs)),
            now
        ));
        let legacy = ["Gemini 3.5 Flash (Low)".to_string()];
        // Legacy human labels from the old TUI scrape: refresh converts them to
        // slugs, which is what the resolvers now expect.
        assert!(models_cache_is_stale(Some((now, &legacy)), now));
    }

    #[test]
    fn test_wire_model() {
        // Argon goes out as the slug: measured on agy 1.3.6 (2026-10-16), `--model
        // gemini-4-argon-medium` was served as "Gemini 4 Argon (Medium)", so the label
        // workaround for #581 is not needed for it (and a label is not in the catalog).
        assert_eq!(wire_model("gemini-4-argon-medium"), "gemini-4-argon-medium");
        assert_eq!(wire_model("gemini-3.1-pro-high"), "Gemini 3.1 Pro (High)");
        assert_eq!(wire_model("gemini-3.1-pro-low"), "Gemini 3.1 Pro (Low)");
        assert_eq!(wire_model("gemini-3.5-flash-low"), "Gemini 3.5 Flash (Low)");
        assert_eq!(
            wire_model("gemini-3.5-flash-medium"),
            "Gemini 3.5 Flash (Medium)"
        );
        assert_eq!(
            wire_model("gemini-3.5-flash-high"),
            "Gemini 3.5 Flash (High)"
        );
        assert_eq!(
            wire_model("gemini-3.6-flash-high"),
            "Gemini 3.6 Flash (High)"
        );
        assert_eq!(wire_model("gemini-3.6-flash"), "Gemini 3.6 Flash");
        assert_eq!(wire_model("gemini-3.1-pro"), "Gemini 3.1 Pro");

        let labels = [
            "Gemini 3.1 Pro (High)",
            "Gemini 3.1 Pro (Low)",
            "Gemini 3.5 Flash (Low)",
            "Gemini 3.6 Flash",
        ];
        for label in labels {
            // Idempotency: wire_model(wire_model(x)) == wire_model(x)
            assert_eq!(wire_model(label), label);
        }

        // A bare-major version converts the same way.
        assert_eq!(wire_model("gemini-4-flash-high"), "Gemini 4 Flash (High)");

        // Unchanged models/inputs. Claude slugs go on the wire as-is: measured
        // 2026-10-03, `claude-sonnet-5-5-low` served "Claude Sonnet 5.5 (Low)".
        assert_eq!(wire_model("claude-sonnet-5-5-low"), "claude-sonnet-5-5-low");
        assert_eq!(wire_model("claude-sonnet-4-6"), "claude-sonnet-4-6");
        assert_eq!(
            wire_model("claude-opus-4-6-thinking"),
            "claude-opus-4-6-thinking"
        );
        assert_eq!(wire_model("gpt-oss-1"), "gpt-oss-1");
        assert_eq!(wire_model("unrecognized-string"), "unrecognized-string");
        assert_eq!(wire_model(""), "");
    }

    #[test]
    fn validate_model_label_empty_cache_accepts_anything() {
        assert!(validate_model_label("nonsense-slug", &[]).is_ok());
        assert!(validate_model_label("gemini-9.9-ultra-max", &[]).is_ok());
    }

    #[test]
    fn validate_model_label_populated_cache_accepts_existing_slug() {
        let cache = vec![
            "gemini-3.7-flash-low".to_string(),
            "gemini-3.7-flash-medium".to_string(),
            "gemini-3.7-flash-high".to_string(),
            "claude-sonnet-4-6".to_string(),
        ];
        assert!(validate_model_label("gemini-3.7-flash-low", &cache).is_ok());
        assert!(validate_model_label("claude-sonnet-4-6", &cache).is_ok());
    }

    #[test]
    fn validate_model_label_populated_cache_rejects_and_names_siblings() {
        let cache = vec![
            "gemini-3.7-flash-low".to_string(),
            "gemini-3.7-flash-medium".to_string(),
            "gemini-3.7-flash-high".to_string(),
            "gemini-3.1-pro-low".to_string(),
        ];
        let err = validate_model_label("gemini-3.7-flash", &cache).unwrap_err();
        assert!(err.contains("gemini-3.7-flash"));
        assert!(err.contains("not in agy's model catalog"));
        assert!(err.contains("gemini-3.7-flash-low"));
        assert!(err.contains("gemini-3.7-flash-medium"));
        assert!(err.contains("gemini-3.7-flash-high"));
        assert!(!err.contains("gemini-3.1-pro-low"));
    }

    #[test]
    fn validate_model_label_populated_cache_rejects_when_family_absent() {
        let cache = vec![
            "gemini-3.7-flash-low".to_string(),
            "gemini-3.7-flash-high".to_string(),
        ];
        let err = validate_model_label("totally-unknown-model", &cache).unwrap_err();
        assert!(err.contains("totally-unknown-model"));
        assert!(err.contains("not in agy's model catalog"));
        assert!(!err.contains("Available siblings"));
    }
}
