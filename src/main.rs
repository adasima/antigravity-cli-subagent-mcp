//! `agy-mcp` — an MCP server that exposes Google's Antigravity CLI (`agy`) as a
//! set of sub-agents for Claude Code (or any MCP host), running on your AI
//! Pro/Ultra subscription quota. Single static binary; no Python, no API key.
//!
//! See `agy.rs` for the why (stdin-close + transcript-read workaround for #76,
//! and the `--model` label / effort details).

mod agy;
mod cli;
mod hookguard;
mod nest;
mod presets;
mod setup;

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    Implementation, InitializeRequestParams, InitializeResult, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData, Peer, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{oneshot, Mutex, OwnedSemaphorePermit, Semaphore};

use agy::RunOpts;

// --- Model catalog slugs (resolved dynamically from cache or fallback constants).
// Catalog slugs are used internally; converted to agy's display-label form at the
// `--model` wire boundary via `wire_model`. Effort is a slug suffix
// (`-high|-medium|-low`) for Gemini and, since Claude 5.5, for Claude too. Bump
// versions here if the cache is empty and Google has rotated. ---
const GEMINI_FLASH: &str = "gemini-3.8-flash";
const GEMINI_PRO: &str = "gemini-4-argon";
const CLAUDE_SONNET: &str = "claude-sonnet-5-5";
const CLAUDE_OPUS: &str = "claude-opus-5-5";

/// The context size past which Flash is **reported** to be out of its comfort
/// zone. ~tokens ≈ transcript bytes / 4.
///
/// This used to be enforced, twice: a resume of a bigger Flash session was
/// refused, and a run that grew past it mid-flight was killed. Both are gone —
/// the threshold now only produces a sentence. Enforcing it spent real, finished
/// work (a killed run's answer, a resume that never happened) to pre-empt a
/// *quality* risk that the caller is better placed to judge than the bridge is:
/// whether this particular conversation is still coherent is visible in the
/// answer, and Flash's usable window is a moving target across generations while
/// this constant is not.
const FLASH_CONTINUE_LIMIT: u64 = 100_000;

/// The Flash context notice threshold in effect (`AGY_CTX_LIMIT`, in tokens).
/// Deliberately ONE knob behind both the resume-time notice and the after-the-run
/// one, so the two can never disagree about how much context Flash holds
/// comfortably. Not an MCP arg (the caller has no better answer than the default,
/// and `force=true` already silences both).
fn flash_ctx_limit() -> u64 {
    std::env::var("AGY_CTX_LIMIT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(FLASH_CONTINUE_LIMIT)
}

/// How long a dispatch waits inline before acking. **`None` — the default — waits
/// for the whole run**, and `AGY_INLINE_WAIT=<secs>` restores the old ack.
///
/// The ack existed because a long call was assumed to block the supervisor, so the
/// window was set at the knee of the run-log curve (25s ≈ 61% of runs). That
/// assumption no longer holds: Claude Code moves a main-conversation MCP call that
/// outlives `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS` (default 2 min) into a background
/// task on its own, hands the model a task id immediately, and delivers the result
/// as a task notification — the same shape this bridge hand-rolled, minus the job
/// id to carry, the uncollected banner and the separate `agy-mcp wait`. **Acking
/// early is what prevented it**: a call that returns at 25s is never backgrounded.
/// So the bridge stops guessing and returns the result; how long the session may
/// block is the client's setting, and only the user knows that number.
///
/// The escape hatch is not decoration. Auto-backgrounding covers **main-conversation
/// calls only** (not subagent-issued ones), it can be switched off, and at least one
/// surface has been reported to time long calls out anyway (claude-code#58687).
/// Anywhere it doesn't apply, `AGY_INLINE_WAIT=25` brings the old behaviour back.
/// Turn this request's progress token, if it has one, into something the run can
/// ping while it works. **Off unless `AGY_PROGRESS` is set**, and that default is
/// the measurement, not caution.
///
/// Reporting progress makes this client abandon the request. Measured 2026-08-15
/// (Claude Desktop, client id `local-agent-mode-agy`), same binary except for the
/// pings, same task shape:
///
/// | pings | outcome |
/// |---|---|
/// | off | 69.3s and 129.6s calls both returned inline |
/// | on  | three calls cut at ~60s with `Request timed out` |
///
/// The last of those had `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS` removed, which is
/// what rules the env var out and leaves the notifications. Nothing is lost when
/// a call dies — the job is detached and `agy-mcp wait` returns the full result —
/// but paying a 60s wall for a status line is a bad trade, so the line goes.
/// Keep the code: the same pings are what a client that honours them needs to
/// avoid the 30-minute idle abort, and this one may be fixed
/// (claude-code#58687 is the same family of report).
///
/// A server may also only send progress for a token the client put in the
/// request's `_meta`, so `None` here is a normal outcome either way.
///
/// Each ping is fire-and-forget on its own task. The run must never be slowed, or
/// worse stalled, by the channel it is reporting on; a dropped notification costs
/// a line of status, and the terminal result still arrives through the response.
fn progress_sink(ctx: &RequestContext<RoleServer>) -> Option<agy::ProgressSink> {
    if !progress_enabled() {
        return None;
    }
    let token = ctx.meta.get_progress_token()?;
    let peer = ctx.peer.clone();
    Some(std::sync::Arc::new(move |n: u32, message: String| {
        let (peer, progress_token) = (peer.clone(), token.clone());
        tokio::spawn(async move {
            let _ = peer
                .notify_progress(
                    rmcp::model::ProgressNotificationParam::new(progress_token, f64::from(n))
                        .with_message(message),
                )
                .await;
        });
    }))
}

/// Whether to emit `notifications/progress` at all. See [`progress_sink`] for the
/// measurement behind the default. Any non-empty value other than `0`/`false`
/// turns it on.
fn progress_enabled() -> bool {
    match std::env::var("AGY_PROGRESS") {
        Ok(v) => {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

fn inline_wait_s(client: Option<&str>) -> Option<u64> {
    match std::env::var("AGY_INLINE_WAIT") {
        // An explicit `0` means "never ack, wait for the run" — the only way to say
        // that, since an absent value now defers to the client profile.
        Ok(v) => v.trim().parse::<u64>().ok().filter(|&n| n > 0),
        Err(_) => client_inline_wait(client),
    }
}

/// How long to wait before acking, for a client we have measured. Pure.
///
/// The bridge would rather return the result than a job id, and on a client that
/// simply holds the call open it does. But some surfaces bound a single request,
/// and once a call is cut the caller has to notice the error, find the job id and
/// collect it — the exact round-trip the ack exists to make cheap and explicit.
/// So where the bound is known, ack safely inside it.
///
/// **`local-agent-mode-*` (Claude Desktop) cuts a call at ~60s.** Measured
/// 2026-08-15: 53s returned inline, 66s and up came back as `Request timed out`,
/// four times, and none of the documented levers moved it — progress notifications
/// off, `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS` unset, `MCP_TOOL_TIMEOUT` raised to an
/// hour. The runs themselves always finished, so nothing was ever lost; what it
/// cost was a surprise error and a manual collection every time.
///
/// **`antigravity-client` (the Antigravity GUI) cuts a call at 180s.** Measured
/// 2026-10-29: a 109s call returned inline; a 5-minute one failed at exactly 3m00s
/// with `MCP tool call to server "agy" timed out after 3m0s`, the run itself still
/// going.
///
/// Add a line here per client rather than a knob per user: which surface bounds a
/// request is a fact about that surface, and the user has no better information
/// than a measurement. `AGY_INLINE_WAIT` still overrides, including `0` for "never".
fn client_inline_wait(client: Option<&str>) -> Option<u64> {
    let name = client?;
    // The reported name is `<surface>-<server>` (e.g. `local-agent-mode-agy`), so
    // match the surface prefix, not the whole string.
    if name.starts_with("local-agent-mode") {
        Some(45)
    } else if name.starts_with("antigravity-client") {
        Some(150)
    } else {
        None
    }
}

/// HARD CEILING on a run (also agy's `--print-timeout` backstop), NOT the primary
/// governor: runs are normally stopped by the liveness/idle timeout (see
/// default_idle_timeout), so this can be generous without risking a runaway — an
/// idle/stuck run dies at the idle cap long before this. A fixed 600s total used to
/// cut off PRODUCTIVE runs the moment they hit the budget (a storage-layer
/// conversion died at 600s while a build it launched was still RUNNING). Nobody
/// waits on this ceiling any more — the caller is released after INLINE_WAIT_S and
/// recovers the result via agy_poll / `agy-mcp wait` — so it's budgeted for the
/// longest run worth finishing rather than for anyone's patience. That is exactly
/// why it is NOT an MCP arg: the caller has no better information than the default.
/// `scripts/` probes drive it through `AGY_TIMEOUT` on the server process instead.
pub(crate) fn timeout_from_env_str(raw: Option<&str>, default_s: u64) -> u64 {
    raw.and_then(|s| s.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default_s)
}

pub(crate) const SUB_TIMEOUT_S: u64 = 3600;
pub(crate) fn timeout_from_env(default_s: u64) -> u64 {
    timeout_from_env_str(std::env::var("AGY_TIMEOUT").ok().as_deref(), default_s)
}

pub(crate) fn default_idle_timeout() -> u64 {
    // Kill agy only after this long with NO conversation-store / brain-dir activity.
    // This is the real governor: it lets agy WAIT on a long `cargo build` it shelled
    // out (observed worst-case conv-dir silence during a build ~37s) without being
    // cut off, while still reaping a genuinely stalled run. Env-tunable.
    std::env::var("AGY_IDLE_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&v: &u64| v >= 30)
        .unwrap_or(180)
}

// Effort is INTERNAL — it is not an `agy_sub` arg. It was one, and the caller
// picking an effort per task is exactly what the user's standing instruction
// forbids; a schema field is an invitation, so the field is gone and the effort is
// a fixed per-model floor (Flash → low, Pro → high; see `effective_effort`),
// matching Flash's cheap-fast-routine positioning. Nothing is lost: the stable
// slugs bake effort into their suffix, so passing an exact `<flash-slug>-high` to
// `model_label` remains the deliberate, explicit override. No version is named
// here on purpose — slugs rotate, and a version in a comment goes stale silently.
#[derive(Default, Clone, Copy)]
enum Effort {
    Low,
    Medium,
    #[default]
    High,
}
impl Effort {
    /// Lowercase slug suffix used by the stable model slugs (`gemini-…-high`).
    fn slug(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
    /// Effort fallback ladder: try the requested one first, then descend by
    /// capability — a version may not offer every effort (3.1 Pro had no Medium).
    fn ladder(self) -> [&'static str; 3] {
        match self {
            Effort::High => ["high", "medium", "low"],
            Effort::Medium => ["medium", "high", "low"],
            Effort::Low => ["low", "medium", "high"],
        }
    }
}

/// Effort for a run: a fixed per-model floor — Flash → **medium**, Pro → high,
/// Sonnet/Opus → **medium**. Claude took no effort until 5.5. Medium because the
/// Claude bucket bills by token cost and is small, and because an agent-written order
/// can carry the judgment itself (the user's call, 2026-10-03; agy's three Claude
/// tiers are taken to match Claude Code's low/medium/high — assumed from use, not
/// measured, since the mapping happens inside Google's backend).
///
/// Flash sat at `low` from 3.5 through 3.7, and that was right *for how work was
/// dispatched then*: the effort floor and the thickness of the spec are one dial, and
/// a low run with every judgment written into the prompt beat a high run with none
/// (measured repeatedly — `docs/ordering-conventions.md`). Raised to medium on
/// 2026-09-09 because the other end of that dial moved: on 3.8 the technical stumbles
/// are largely gone, so the standard dispatch is becoming "intent, UX and constraints"
/// rather than a full spec, and a floor of `low` under-serves a prompt that
/// deliberately leaves the implementation open.
///
/// The one datum against medium — "same quality as high, worst time and steps of any
/// arm" — is a **3.6-generation n=1** (`ordering-conventions.md`). It is not evidence
/// about 3.8, and it is not why this is set here; if a 3.8 measurement reproduces it,
/// move the floor rather than arguing with the user's usage.
fn effective_effort(model: SubModel) -> Effort {
    slot_effort(Some(match model {
        SubModel::Flash => agy::Slot::Flash,
        SubModel::Pro => agy::Slot::Pro,
        SubModel::Sonnet | SubModel::Opus => agy::Slot::Claude,
    }))
}

/// [`effective_effort`] by slot, for a model reached by name: the same floor for a
/// slot model, and medium for a slot-less one (GPT-OSS lists medium only).
fn slot_effort(slot: Option<agy::Slot>) -> Effort {
    match slot {
        Some(agy::Slot::Pro) => Effort::High,
        _ => Effort::Medium,
    }
}

/// Which Claude model to use (both run on your Antigravity subscription quota).
#[derive(Clone, Copy)]
enum ClaudeModel {
    Sonnet,
    Opus,
}
impl ClaudeModel {
    fn label(self) -> &'static str {
        match self {
            ClaudeModel::Sonnet => CLAUDE_SONNET,
            ClaudeModel::Opus => CLAUDE_OPUS,
        }
    }
}

/// The four `model=` aliases. No longer the arg's type — `model` takes any name — but
/// an alias still wins over name resolution, so `pro` stays the nominal top Gemini
/// line (Argon today) and the existing calls and the guide keep their meaning.
#[derive(Default, Clone, Copy)]
enum SubModel {
    #[default]
    Flash,
    Pro,
    Sonnet,
    Opus,
}
impl SubModel {
    /// The alias a `model` value names (case-insensitive), or None for a model name.
    fn from_alias(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "flash" => Some(SubModel::Flash),
            "pro" => Some(SubModel::Pro),
            "sonnet" => Some(SubModel::Sonnet),
            "opus" => Some(SubModel::Opus),
            _ => None,
        }
    }
}

/// The preset used when the caller names none. A real entry in the table, not a
/// special case — so a project that wants every bare ask to carry its house rules
/// redefines `none` and gets it, with no arg to remember.
const DEFAULT_PRESET: &str = "none";

/// Prepend the resolved preamble (if any) to the user's prompt, substituting the
/// paths a preset can name. Pure — the substitution values are passed in, so the
/// wording a run actually receives is testable without a filesystem.
fn with_preamble(preamble: &str, workspace: &Path, reports: &str, user: String) -> String {
    if preamble.is_empty() {
        return user;
    }
    let pre = preamble
        .replace("{reports}", reports)
        .replace("{workspace}", &workspace.to_string_lossy());
    format!("{pre}\n\n---\nTask:\n{user}")
}

/// Make the passed workspace authoritative for file creation. agy has no project
/// flag and its tool-runner defaults to its own scratch dir even when cwd is the
/// workspace, so we state the absolute path explicitly (agy honors absolute paths).
///
/// The shell line is here rather than in a preset because it applies to EVERY
/// run: headless agy soft-denies un-granted commands, and the bridge can only
/// auto-grant a bare build/test-family invocation. A wrapped command
/// (`powershell -Command "(cargo help build)[0]"` — observed) is unparseable to
/// the grant rule, so it can never be recovered and simply kills the run.
fn prepend_workspace_directive(ws: &Path, body: String) -> String {
    use agy::{WORKSPACE_DIRECTIVE_END, WORKSPACE_DIRECTIVE_HEAD};
    format!(
        "{WORKSPACE_DIRECTIVE_HEAD}{}\nCreate and edit ALL files under that \
         absolute path; do NOT use a scratch or temporary directory.\nRun shell commands \
         DIRECTLY (e.g. `cargo test`, `npm run build`) — never wrap them in `powershell \
         -Command`/`cmd /c`, and never use pipes or output redirection: wrapped or piped \
         commands cannot be auto-approved here and will fail the run.\nTo read, search, \
         summarize, CREATE or EDIT files, use your own file tools — do NOT shell out to an \
         inline script runner (`python -c`, `node -e`, ...) and do NOT write files through the \
         shell (`Set-Content`, `Out-File`, `echo >`, a here-string): an arbitrary script can \
         never be auto-approved, and a shell write is inspected as if the file's CONTENT were \
         commands, so a document that merely mentions one gets refused.\nUnless the task names \
         such a file, do not write report, summary or notes files inside the project \
         directory; put the report in the final reply.{WORKSPACE_DIRECTIVE_END}{body}",
        ws.display()
    )
}

/// Remove `default` keys from every entry under the schema's top-level
/// `properties`. `default` is annotation-only in JSON Schema, but Claude
/// Desktop (claude-code#94718) rejects any omitted property whose schema
/// carries one with `expected nonoptional, received undefined`. `nullable`
/// and everything else is kept (Gemini function calling uses `nullable`).
/// `#[serde(default)]` still does the real defaulting at deserialization.
fn strip_defaults(schema: &mut schemars::Schema) {
    if let Some(props) = schema.as_object_mut().and_then(|o| {
        o.get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
    }) {
        for (_key, prop_schema) in props.iter_mut() {
            if let Some(obj) = prop_schema.as_object_mut() {
                obj.remove("default");
            }
        }
    }
}

/// Normalize an optional string arg: trim whitespace; `""` or `"null"`
/// (case-insensitive) becomes None. Some MCP clients turn a JSON null into
/// the string `"null"` (claude-code#81911) or send an empty string for an
/// omitted arg; without this, `preset: ""` fails as an unknown preset name
/// and `model_label: "null"` fails catalog validation.
fn normalize_opt_arg(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"))
}

#[derive(Deserialize, JsonSchema)]
#[schemars(transform = strip_defaults)]
struct SubArgs {
    /// The task or question to run.
    prompt: String,
    /// The target model: flash (default), pro, sonnet, opus, or any model name.
    #[serde(default)]
    model: Option<String>,
    /// Exact model slug to bypass resolution (also pins version + effort).
    #[serde(default)]
    model_label: Option<String>,
    /// Session ID to resume an existing conversation.
    #[serde(default)]
    conversation_id: Option<String>,
    /// Absolute path to the workspace directory. Omit only if your client offers
    /// MCP roots (a single one); otherwise required.
    #[serde(default)]
    workspace: Option<String>,
    /// Additional directories to grant file access.
    #[serde(default)]
    add_dirs: Vec<String>,
    /// Configured MCP servers to inherit (default), "none", or a comma-separated list.
    #[serde(default)]
    mcp: Option<String>,
    /// Bypass the quota and context-size guardrails (default false).
    #[serde(default)]
    force: bool,
    /// Preset preamble: none (default), code, research, report, or a name defined in
    /// ~/.agy-mcp/presets.toml or <workspace>/.agy/presets.toml.
    #[serde(default)]
    preset: Option<String>,
    /// Paths this run may write (workspace-relative prefixes); writes outside are
    /// flagged in the footer. Replaces the preset's.
    #[serde(default)]
    scope: Option<Vec<String>>,
}
/// Answer cap. Not an arg: the caller has no basis for picking a number, and an
/// over-cap answer spills to a file rather than being lost. Env-overridable for
/// probes.
fn default_max_answer() -> usize {
    std::env::var("AGY_MAX_ANSWER_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16_000)
}

#[derive(Deserialize, JsonSchema)]
#[schemars(transform = strip_defaults)]
struct PollArgs {
    /// Job ID to poll. Omit for the most recent job.
    #[serde(default)]
    job_id: Option<String>,
}

// Variants deliberately undocumented (schema bloat) — the action field
// description enumerates them.
#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum DiagAction {
    Status,
    Mcp,
    Trace,
    Sessions,
    Live,
}

#[derive(Deserialize, JsonSchema, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum LiveWhat {
    #[default]
    Quota,
    Models,
    Both,
}

#[derive(Deserialize, JsonSchema)]
#[schemars(transform = strip_defaults)]
struct DiagArgs {
    /// status (bridge/agy versions) | mcp (server list) | trace (step trace of a run) |
    /// sessions (recent conversations) | live (fresh quota/models from agy, headless).
    action: DiagAction,
    /// trace: conversation ID (default = most recent run).
    #[serde(default)]
    conversation_id: Option<String>,
    /// trace: include full model thinking (default false).
    #[serde(default)]
    full: bool,
    /// sessions: max entries (default 12).
    #[serde(default)]
    limit: Option<usize>,
    /// live: quota (default), models, or both.
    #[serde(default)]
    what: LiveWhat,
}

/// Resolve a Gemini slot to the current `--model` slug. Cache-first (so we follow
/// Google's version rotations). Among the slot's slugs it prefers the NEWEST
/// version (3.6 > 3.5) unless `ver_pin` names one, then picks the nearest
/// available effort down the ladder. Returns None when the cache is non-empty but
/// holds NO slug for this slot (rotation the classifier didn't anticipate) OR the
/// pinned version isn't present — the caller surfaces the right hint. The const is
/// used only when the cache is empty (never refreshed).
fn resolve_gemini(slot: agy::Slot, effort: Effort, ver_pin: Option<(u32, u32)>) -> Option<String> {
    resolve_gemini_in(&agy::load_models_cache(), slot, effort, ver_pin)
}

/// [`resolve_gemini`] over a given catalog: the const only when it is empty.
fn resolve_gemini_in(
    labels: &[String],
    slot: agy::Slot,
    effort: Effort,
    ver_pin: Option<(u32, u32)>,
) -> Option<String> {
    if labels.is_empty() {
        let base = match slot {
            agy::Slot::Pro => GEMINI_PRO,
            _ => GEMINI_FLASH,
        };
        return Some(format!("{base}-{}", effort.slug()));
    }
    resolve_gemini_from(labels, slot, effort, ver_pin)
}

/// Pure core of `resolve_gemini` (cache passed in) so the version-preference and
/// effort-ladder behavior is unit-testable. Returns None when the slot is absent
/// (rotation) or the pinned version isn't present.
fn resolve_gemini_from(
    labels: &[String],
    slot: agy::Slot,
    effort: Effort,
    ver_pin: Option<(u32, u32)>,
) -> Option<String> {
    let cands: Vec<&String> = labels
        .iter()
        .filter(|l| agy::slot_of(l) == Some(slot))
        .collect();
    pick_version_effort(&cands, effort, ver_pin)
}

/// Resolve a model *name* to one catalog slug. In order: (1) the exact slug;
/// (2) the same [`agy::family_key`] (`gemini-4-argon`, "Gemini 4 Argon (Low)");
/// (3) every word of the name is a word of exactly ONE model line — a line being a
/// family without its version numbers, so `sonnet` over 4.6 and 5.5 is one line and
/// takes the newest. Two or more lines is an error listing them, never a guess.
///
/// Effort: an effort the name ends in is honored exactly (missing → error naming
/// what the family has); otherwise the slot's floor ([`slot_effort`]) down the
/// ladder. Nothing matching → [`agy::validate_model_label`]'s catalog-miss message.
/// An empty catalog passes the name through, as `validate_model_label` does.
fn resolve_by_name(name: &str, labels: &[String]) -> Result<String, String> {
    if labels.is_empty() || labels.iter().any(|l| l == name) {
        return Ok(name.to_string());
    }
    let key = agy::family_key(name);
    let named_effort = name_effort(name);
    let line = |fam: &str| -> String {
        fam.split('-')
            .filter(|t| !t.chars().all(|c| c.is_ascii_digit()))
            .collect::<Vec<_>>()
            .join("-")
    };

    let mut cands: Vec<&String> = labels
        .iter()
        .filter(|l| agy::family_key(l) == key)
        .collect();
    if cands.is_empty() {
        let words: Vec<&str> = key.split('-').collect();
        let hits: Vec<&String> = labels
            .iter()
            .filter(|l| {
                let fam = agy::family_key(l);
                let fam_toks: Vec<&str> = fam.split('-').collect();
                words.iter().all(|w| fam_toks.contains(w))
            })
            .collect();
        let mut lines: Vec<String> = hits.iter().map(|l| line(&agy::family_key(l))).collect();
        lines.sort();
        lines.dedup();
        if lines.len() > 1 {
            let mut bases: Vec<String> = hits.iter().map(|l| agy::base_slug(l)).collect();
            bases.sort();
            bases.dedup();
            return Err(format!(
                "'{name}' matches more than one model: {}. Name one of them.",
                bases.join(", ")
            ));
        }
        cands = hits;
    }
    if cands.is_empty() {
        return agy::validate_model_label(name, labels).map(|()| name.to_string());
    }

    let effort = named_effort.unwrap_or_else(|| slot_effort(agy::slot_of(cands[0])));
    let picked = pick_version_effort(&cands, effort, None)
        .ok_or_else(|| format!("'{name}' resolved to no model (internal)"))?;
    if named_effort.is_some() && !picked.ends_with(&format!("-{}", effort.slug())) {
        let fam = agy::family_key(&picked);
        let mut have: Vec<&str> = cands
            .iter()
            .filter(|l| agy::family_key(l) == fam)
            .map(|l| l.as_str())
            .collect();
        have.sort();
        return Err(format!(
            "'{name}' is not in agy's model catalog. Available in family '{fam}': {}.",
            have.join(", ")
        ));
    }
    Ok(picked)
}

/// The effort a model name ends in (`-low`, " (High)"), by the same rule
/// [`agy::family_key`] uses to drop it; None for a bare name.
fn name_effort(name: &str) -> Option<Effort> {
    let lower = name.to_ascii_lowercase();
    let toks: Vec<&str> = lower
        .split(|c: char| c == '.' || c == '-' || c == '(' || c == ')' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .collect();
    if toks.len() < 2 {
        return None;
    }
    match *toks.last()? {
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        _ => None,
    }
}

/// [`resolve_by_name`] against the cache, re-listing once on a miss before it is
/// believed — the same shape as [`resolve_sub_refreshing`], and for the same reason:
/// the cache self-heals, so the caller is never told to go refresh it (invariant #4).
async fn resolve_by_name_refreshing(name: &str) -> Result<String, String> {
    if let Ok(hit) = resolve_by_name(name, &agy::load_models_cache()) {
        return Ok(hit);
    }
    let refreshed = agy::refresh_models_cache().await;
    resolve_by_name(name, &agy::load_models_cache()).map_err(|e| {
        let note = if refreshed {
            "the model list was auto-refreshed for this call"
        } else {
            "auto-refresh of the model list found nothing new"
        };
        format!("{e} ({note})")
    })
}

/// Shared tail of the Gemini and Claude resolvers: among one family's slugs, take
/// the pinned version (else the newest present — 3.6 over 3.5, 4 over 3.8), then the
/// nearest effort down the ladder. None when `cands` is empty (rotated/renamed) or
/// the pinned version is absent.
fn pick_version_effort(
    cands: &[&String],
    effort: Effort,
    ver_pin: Option<(u32, u32)>,
) -> Option<String> {
    if cands.is_empty() {
        return None; // cache has models, just not this slot => rotated/renamed
    }
    let target_ver = ver_pin.or_else(|| cands.iter().filter_map(|l| agy::model_version(l)).max());
    let vcands: Vec<&&String> = match target_ver {
        Some(v) => cands
            .iter()
            .filter(|l| agy::model_version(l) == Some(v))
            .collect(),
        None => cands.iter().collect(),
    };
    if vcands.is_empty() {
        return None; // pinned version not in the cache
    }
    // Effort suffix, both slug (`-high`) and legacy human-label (`(high)`) forms.
    for e in effort.ladder() {
        let dash = format!("-{e}");
        let paren = format!("({e})");
        if let Some(l) = vcands.iter().find(|l| {
            let low = l.to_ascii_lowercase();
            low.ends_with(&dash) || low.ends_with(&paren)
        }) {
            return Some((***l).clone());
        }
    }
    vcands.first().map(|l| (***l).clone()) // version present but no effort suffix
}

/// Resolve a Claude variant to its current slug. None if the cache is non-empty
/// but holds no matching Claude slug (rotated); const only when cache is empty.
fn resolve_claude(which: ClaudeModel, effort: Effort) -> Option<String> {
    resolve_claude_in(&agy::load_models_cache(), which, effort)
}

/// [`resolve_claude`] over a given catalog: the const only when it is empty.
fn resolve_claude_in(labels: &[String], which: ClaudeModel, effort: Effort) -> Option<String> {
    if labels.is_empty() {
        return Some(format!("{}-{}", which.label(), effort.slug()));
    }
    resolve_claude_from(labels, which, effort)
}

/// Pure core of [`resolve_claude`]. Same rule as Gemini — newest version, then the
/// effort ladder — because since 5.5 the Claude slugs carry the effort suffix too.
/// Taking the first match instead (as this did before) resolved `sonnet` to
/// `claude-sonnet-5-5-low`, the first row agy lists.
fn resolve_claude_from(labels: &[String], which: ClaudeModel, effort: Effort) -> Option<String> {
    let key = match which {
        ClaudeModel::Sonnet => "sonnet",
        ClaudeModel::Opus => "opus",
    };
    let cands: Vec<&String> = labels
        .iter()
        .filter(|l| {
            agy::slot_of(l) == Some(agy::Slot::Claude) && l.to_ascii_lowercase().contains(key)
        })
        .collect();
    pick_version_effort(&cands, effort, None)
}

/// Resolve the `agy_sub` model arg to (full slug, slot). Err = the slot rotated out
/// of the cache; the message tells the caller how to recover.
///
/// Deliberately takes NO version pin: `agy_sub` has no `gemini_ver` arg (a slug
/// names its own version, so `model_label` is the pin), and the one caller that
/// really does pin a version — [`resume_keep_model`], holding a conversation to
/// the Gemini generation it started on — calls [`resolve_gemini`] directly.
/// A `model_label` as the caller meant it: everything before the first TAB.
///
/// `agy_diag(live, what=models)` prints `<slug>\t<display label>`, and the guidance
/// says to take the slug from there **verbatim** — so a pasted row arrives as both
/// halves joined by a tab. Two runs died exactly that way (run log, 2026-08-14:
/// `gemini-3.7-flash-low\tGemini 3.7 Flash (Low)`, ok=false after ~6s). A tab
/// occurs in neither a slug nor a display label, so cutting there cannot damage a
/// legitimate value — which is why this repairs silently instead of erroring.
fn model_label_slug(raw: &str) -> &str {
    raw.split('\t').next().unwrap_or(raw).trim()
}

fn resolve_sub(model: SubModel, effort: Effort) -> Result<(String, agy::Slot), String> {
    let (opt, slot, name) = match model {
        SubModel::Flash => (
            resolve_gemini(agy::Slot::Flash, effort, None),
            agy::Slot::Flash,
            "flash",
        ),
        SubModel::Pro => (
            resolve_gemini(agy::Slot::Pro, effort, None),
            agy::Slot::Pro,
            "pro",
        ),
        SubModel::Sonnet => (
            resolve_claude(ClaudeModel::Sonnet, effort),
            agy::Slot::Claude,
            "sonnet",
        ),
        SubModel::Opus => (
            resolve_claude(ClaudeModel::Opus, effort),
            agy::Slot::Claude,
            "opus",
        ),
    };
    opt.map(|l| (l, slot)).ok_or_else(|| {
        format!(
            "no '{name}' model in the cached list — Google may have rotated the model names. \
             Run agy_diag(action=live, what=models) to refresh, then retry; or pass model_label \
             with an exact slug from that list."
        )
    })
}

/// [`resolve_sub`], but a miss re-reads the model list from agy first and tries
/// once more. `agy models` is a headless subcommand: zero quota, no TUI, ~2s —
/// so a rotation the cache hasn't seen yet is something the bridge can just fix,
/// instead of failing and telling the caller to run agy_diag(live, what=models)
/// by hand. Only a miss that survives a *fresh* list is a real error.
async fn resolve_sub_refreshing(
    model: SubModel,
    effort: Effort,
) -> Result<(String, agy::Slot), String> {
    let first = match resolve_sub(model, effort) {
        Ok(hit) => return Ok(hit),
        Err(e) => e,
    };
    if !agy::refresh_models_cache().await {
        return Err(format!(
            "{first} (auto-refresh of the model list found nothing new)"
        ));
    }
    resolve_sub(model, effort)
        .map_err(|e| format!("{e} (the model list was already auto-refreshed for this call)"))
}

/// Resolve the `model` arg: an alias (`flash|pro|sonnet|opus`) through the slot
/// resolvers first, anything else as a name ([`resolve_by_name`]). Both re-list
/// the catalog once on a miss.
async fn resolve_model_arg(m: &str) -> Result<String, String> {
    match SubModel::from_alias(m) {
        Some(sm) => resolve_sub_refreshing(sm, effective_effort(sm))
            .await
            .map(|(l, _)| l),
        None => resolve_by_name_refreshing(m).await,
    }
}

/// `AGY_DEFAULT_MODEL`: the model a fresh run takes when the caller names none. A
/// setup-time env on the registration (like `AGY_PERMISSIONS`), never a tool arg.
fn default_model_env() -> Option<String> {
    normalize_opt_arg(std::env::var("AGY_DEFAULT_MODEL").ok())
}

/// The model of a fresh run with no `model`/`model_label`: `AGY_DEFAULT_MODEL` if
/// set, else the `flash` alias. Never a guess past that — when neither resolves,
/// the run is refused ([`no_default_model_text`]) rather than handed to whichever
/// model happens to be first in the catalog.
async fn resolve_default_model() -> Result<String, String> {
    match default_model_env() {
        Some(name) => resolve_model_arg(&name)
            .await
            .map_err(|e| format!("AGY_DEFAULT_MODEL={name}: {e}")),
        None => resolve_model_arg("flash")
            .await
            .map_err(|e| no_default_model_text(&e, &agy::load_models_cache())),
    }
}

/// The refusal when no default model resolves: what failed, the catalog's model
/// lines, and how to set one. Pure.
fn no_default_model_text(err: &str, labels: &[String]) -> String {
    let mut bases: Vec<String> = labels.iter().map(|l| agy::base_slug(l)).collect();
    bases.sort();
    bases.dedup();
    let have = if bases.is_empty() {
        "(empty)".to_string()
    } else {
        bases.join(", ")
    };
    format!(
        "no default model: {err}. Pass model=<name> (catalog: {have}), or set a default \
         with -e AGY_DEFAULT_MODEL=<name> on the agy-mcp registration."
    )
}

/// `agy-mcp doctor`'s line for the default model. The CLI sees its own shell's env,
/// not the registration's `-e`, so it says which it read.
fn default_model_line(env: Option<String>) -> String {
    match env {
        Some(m) => format!("default model: {m} (AGY_DEFAULT_MODEL in this shell; the server reads its own registration's -e)"),
        None => "default model: flash (AGY_DEFAULT_MODEL unset in this shell; the server reads its own registration's -e)".to_string(),
    }
}

/// Extract the effort tier from a model label/slug (`… (High)` or `…-high`),
/// defaulting to high when absent — used to preserve a resumed session's effort.
fn effort_from_label(label: &str) -> Effort {
    let l = label.to_ascii_lowercase();
    if l.contains("(low)") || l.ends_with("-low") {
        Effort::Low
    } else if l.contains("(medium)") || l.ends_with("-medium") {
        Effort::Medium
    } else {
        Effort::High
    }
}

/// Which Claude variant a label names (Opus if it says so, else Sonnet).
fn claude_variant_from_label(label: &str) -> ClaudeModel {
    if label.to_ascii_lowercase().contains("opus") {
        ClaudeModel::Opus
    } else {
        ClaudeModel::Sonnet
    }
}

/// On a **pure resume** (no explicit model/model_label), keep the conversation on
/// its OWN model instead of letting agy silently default to Flash (observed: a
/// Sonnet thread dropping to Flash on resume). Re-resolves the session's slot to a
/// current slug, preserving its effort + Gemini version. Returns None for a Flash
/// or unreadable session — Flash needs no override (agy's resume default is already
/// Flash) and an unknown label falls back to that same default (no regression).
fn resume_keep_model(dir: &Path) -> Option<String> {
    let label = agy::session_model(dir)?;
    resume_keep_from(&label, &agy::load_models_cache())
}

/// Pure core of [`resume_keep_model`]: the served label and the catalog in hand.
///
/// A slot-less model (GPT-OSS, a codename `CODENAME_SLOTS` doesn't know) is kept by
/// its [`agy::family_key`]: the catalog's slugs of the same family, at the session's
/// effort. Without this a bare resume fell through to agy's default, Flash. Not in
/// the catalog → None, the old behavior.
fn resume_keep_from(label: &str, labels: &[String]) -> Option<String> {
    let effort = effort_from_label(label);
    match agy::slot_of(label) {
        Some(agy::Slot::Flash) => None,
        Some(agy::Slot::Pro) => {
            resolve_gemini_in(labels, agy::Slot::Pro, effort, agy::model_version(label))
        }
        // No version pin: Claude generations replace each other in agy's catalog
        // (4.6 vanished when 5.5 arrived), so pinning would resolve to nothing.
        Some(agy::Slot::Claude) => {
            resolve_claude_in(labels, claude_variant_from_label(label), effort)
        }
        None => {
            let fam = agy::family_key(label);
            let cands: Vec<&String> = labels
                .iter()
                .filter(|l| agy::family_key(l) == fam)
                .collect();
            pick_version_effort(&cands, effort, None)
        }
    }
}

/// What a resume needs to know about the conversation on disk. The caller gathers
/// it so [`plan_dispatch`] can stay pure: the disk reads (`session_ctx_tokens`,
/// `session_model`, [`resume_keep_model`]) happen once, outside the decision, and
/// the resume/switch/flash-guard branches become unit-testable.
#[derive(Debug, Default)]
struct ResumeFacts {
    /// The conversation dir's own name — the id agy resumes by.
    conv_id: String,
    /// Persisted transcript size in ~tokens (bytes/4).
    ctx_tokens: u64,
    /// The session's ACTUAL model, per the transcript receipt (not what was asked).
    session_model: Option<String>,
    /// What a bare resume must pass `--model` to hold the session on its own model
    /// ([`resume_keep_model`]); None for a Flash or unreadable session. Only read
    /// when no switch was requested.
    keep_model: Option<String>,
}

/// The model/resume decisions of one `agy_sub` dispatch.
#[derive(Debug, PartialEq)]
struct DispatchPlan {
    /// `--model` to pass agy: the switch target, the kept model, or the fresh
    /// label. None means "let agy pick" (a bare resume of a Flash session).
    model: Option<String>,
    /// Conversation to resume; None starts fresh.
    continue_id: Option<String>,
    /// How the run is labelled in the job record and the report.
    model_disp: String,
    /// Context advisory known at dispatch time (the session was already big),
    /// prepended to the report. Never blocks the run.
    ctx_warn: Option<String>,
    /// Threshold for the after-the-run context notice ([`ctx_limit_for`]); None =
    /// say nothing.
    ctx_limit: Option<u64>,
}

/// The context size past which a run on `model` gets a notice, or None for no
/// notice at all.
///
/// Flash only, and only when not forced — deliberately the same slot, the same
/// threshold and the same opt-out as the resume-time notice, so "how much context
/// does Flash hold comfortably" has ONE answer whether the context was already
/// there or grew during the run. `model=None` means agy picks and agy's own
/// default is Flash, so that gets the notice too. An unclassifiable label gets
/// none (same rule as `slot_mismatch`: can't classify → don't act). Pro says
/// nothing on purpose: a 224.5k-ctx Pro run finished fine (2026-07-26).
fn ctx_limit_for(model: Option<&str>, force: bool) -> Option<u64> {
    if force {
        return None;
    }
    match model {
        None => Some(flash_ctx_limit()),
        Some(m) => (agy::slot_of(m) == Some(agy::Slot::Flash)).then_some(flash_ctx_limit()),
    }
}

/// The after-the-run context notice: what a mid-run ctx watchdog used to *kill* a
/// run to say. Pure.
///
/// Reported once the answer is in hand, so the run's work is never the price of
/// the warning. No model is named to switch to: every line degrades with length,
/// so switching models is not the fix; a fresh conversation is.
fn ctx_over_note(ctx_tokens: u64, limit: Option<u64>) -> Option<String> {
    let limit = limit?;
    (ctx_tokens > limit).then(|| {
        format!(
            "⚠ this conversation is now ~{}tok, past the ~{}tok this model holds comfortably — \
             answers degrade on long contexts. If it starts drifting, start a fresh conversation \
             with a short handoff summary; if the task is done, just start fresh.",
            agy::fmt_tok(ctx_tokens),
            agy::fmt_tok(limit),
        )
    })
}

/// Decide model + resume for one dispatch. Pure, so every branch below is covered
/// by `cargo test` instead of only by a live MCP call.
///
/// `switch` is the caller-resolved model label: for a fresh run it is the resolved
/// slug (always present), for a resume it is Some only when the caller asked to
/// SWITCH the conversation's model. `facts` is Some for a resume. Err is reserved
/// for an internal inconsistency — no context size refuses a dispatch any more.
fn plan_dispatch(
    switch: Option<String>,
    facts: Option<ResumeFacts>,
    force: bool,
) -> Result<DispatchPlan, String> {
    let Some(f) = facts else {
        // Fresh run: the label was resolved before we got here.
        let label = switch
            .ok_or_else(|| "internal: no model label resolved for a fresh run".to_string())?;
        return Ok(DispatchPlan {
            model: Some(label.clone()),
            continue_id: None,
            model_disp: label.clone(),
            ctx_limit: ctx_limit_for(Some(&label), force),
            ctx_warn: None,
        });
    };

    // Context notices for a resume. Neither of them blocks the dispatch: this used
    // to REFUSE a heavy Flash resume, which spent a whole conversation's context to
    // pre-empt a quality risk the caller can see in the answer (see
    // FLASH_CONTINUE_LIMIT). Most specific first — the Flash notice is the more
    // specific one; the generic one only names the ceiling. A switch off Flash is
    // silent because the Flash threshold does not describe the new model.
    let switching_off_flash = switch
        .as_deref()
        .and_then(agy::slot_of)
        .is_some_and(|s| s != agy::Slot::Flash);
    let staying_on_flash = !switching_off_flash
        && f.session_model.as_deref().and_then(agy::slot_of) == Some(agy::Slot::Flash);
    let ctx_warn = (!force && staying_on_flash)
        .then(|| ctx_over_note(f.ctx_tokens, Some(flash_ctx_limit())))
        .flatten()
        .or_else(|| {
            (f.ctx_tokens > agy::CLAUDE_CTX_CEILING).then(|| {
                format!(
                "⚠ session ctx ~{}tok exceeds the ~200k measured input ceiling — consider starting fresh",
                agy::fmt_tok(f.ctx_tokens)
            )
            })
        });

    // Model to pass agy on resume: an explicit switch if given, else KEEP the
    // conversation's own model (don't let agy default to Flash). Either way
    // run_core verifies the served slot against this label.
    //
    // The display names the model even on a bare resume: `model_disp` used to be
    // the bare string "(resume)" no matter which model that actually was, so a
    // supervisor running several jobs in parallel could not tell a Flash resume
    // from a Sonnet one from the job listing alone. Prefer the KEPT model (what
    // will actually be passed to agy); fall back to the session's own recorded
    // model when there's no keep (a Flash session, or one we couldn't read) so a
    // Flash resume still says "Flash" instead of nothing; only bare "(resume)"
    // when neither is known.
    let (model, model_disp) = match &switch {
        Some(l) => (Some(l.clone()), format!("(resume→{l})")),
        None => {
            let shown = f.keep_model.as_deref().or(f.session_model.as_deref());
            let disp = match shown {
                Some(m) => format!("(resume: {m})"),
                None => "(resume)".to_string(),
            };
            (f.keep_model.clone(), disp)
        }
    };
    let ctx_limit = ctx_limit_for(model.as_deref(), force);
    Ok(DispatchPlan {
        model,
        continue_id: Some(f.conv_id),
        model_disp,
        ctx_warn,
        ctx_limit,
    })
}

/// The refusal when agy cannot run at all. Fixed prefix, like `AGY_UNAVAILABLE:`,
/// so a caller's rule can switch to doing the work itself without parsing prose.
fn agy_down_text(why: &str) -> String {
    format!(
        "AGY_DOWN: {why}\nThe zero-quota probe (`agy models`) failed, so no model can \
         run. Check the agy install (agy_diag(action=status)); force=true tries anyway."
    )
}

/// Parse the `mcp` arg: omit/"inherit" => no swap; "none" => empty; else a
/// comma-separated allow-list of server names.
fn parse_mcp(s: Option<String>) -> agy::McpMode {
    match s.as_deref().map(str::trim) {
        None | Some("") | Some("inherit") => agy::McpMode::Inherit,
        Some("none") => agy::McpMode::None,
        Some(list) => agy::McpMode::Only(
            list.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect(),
        ),
    }
}

fn to_err(e: anyhow::Error) -> ErrorData {
    // `{:#}` includes the whole anyhow source chain (e.g. the OS error behind a
    // spawn failure), not just the top-level context — far better diagnostics.
    ErrorData::internal_error(format!("{e:#}"), None)
}

/// Seconds since the Unix epoch for a `SystemTime` (0 if before the epoch).
fn unix(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format a job's status line. Shared by MCP agy_poll (in-memory) and the disk
/// fallback so the two never drift. `status` is "running" | "done" | "failed".
// The arguments ARE the status line's fields; grouping them into a struct would just be the
// same list with a name on it, and the two callers hold them in different shapes anyway.
#[allow(clippy::too_many_arguments)]
pub(crate) fn format_job_detail(
    id: &str,
    tool: &str,
    model: &str,
    status: &str,
    secs: u64,
    result: Option<&str>,
    prompt_head: &str,
    silent_s: Option<u64>,
    last_step: Option<&str>,
    waited: bool,
) -> String {
    match status {
        "done" => format!(
            "job {id} [{tool}] done in {secs}s · model {model}:\n\n{}",
            result.unwrap_or("(no output)")
        ),
        "failed" => format!(
            "job {id} [{tool}] FAILED after {secs}s:\n{}",
            result.unwrap_or("(no detail)")
        ),
        // A running job is only good news while agy is actually writing. Surface
        // the silence explicitly: a run that has shown no conversation activity
        // for longer than the idle timeout is stalled, not progressing (observed:
        // a job reported "running" for 29 min after agy went silent at +23s).
        _ => {
            let liveness = match silent_s {
                Some(s) if s > default_idle_timeout() => {
                    format!(" · ⚠ NO agy activity for {s}s — likely stalled; consider agy_poll again shortly, then treat it as dead")
                }
                Some(s) => format!(" · last agy activity {s}s ago"),
                None => String::new(),
            };
            // The current step, when the job-mirror tick has landed one: without
            // it, every running job shows the same generic preamble head
            // (`prompt_head`), and a supervisor running several in parallel can't
            // tell which one is stuck on what.
            let now_step = match last_step {
                Some(s) => format!(" · now: {s}"),
                None => String::new(),
            };
            format!(
                "job {id} [{tool}] running {secs}s · model {model}{liveness} · \"{prompt_head}\"{now_step}\n{}",
                // "poll again later" read as an order while a background wait was
                // already watching the job, and the caller kept polling it.
                if waited {
                    "(a background `agy-mcp wait` is watching it — no need to poll; you are woken when it ends)"
                } else {
                    "(poll again later)"
                }
            )
        }
    }
}

/// Shorten an artifact path: workspace-relative if possible, else basename.
fn short_path(p: &str, ws: &Path) -> String {
    let pb = Path::new(p);
    if let Ok(rel) = pb.strip_prefix(ws) {
        return rel.to_string_lossy().into_owned();
    }
    pb.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

/// Workspace-relative path list, capped at 5 with `(+N more)`. One formatter for
/// both the success footer and the failure path, so the two can't drift.
fn fmt_paths_capped(paths: &[String], ws: &Path) -> String {
    let shown: Vec<String> = paths.iter().take(5).map(|p| short_path(p, ws)).collect();
    let mut s = shown.join(", ");
    if paths.len() > 5 {
        s.push_str(&format!(" (+{} more)", paths.len() - 5));
    }
    s
}

/// [`fmt_paths_capped`], with a per-path `wrote:` note (the line-count delta
/// from `gather_wrote_notes`) appended in parens where one is available —
/// `wrote: docs/DESIGN.md (new, 78 lines), docs/REFERENCE.md (1662→1589
/// lines)` (docs/feedback-2026-09-15 P1). `notes` is aligned by index with
/// `paths`; pure, since the notes themselves are already-decided data, not
/// computed here — that keeps this formatter unit-testable without a real
/// repo, same as `fmt_paths_capped`.
fn fmt_paths_capped_with_notes(paths: &[String], ws: &Path, notes: &[Option<String>]) -> String {
    let shown: Vec<String> = paths
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, p)| {
            let short = short_path(p, ws);
            match notes.get(i).and_then(|n| n.as_ref()) {
                Some(note) => format!("{short} ({note})"),
                None => short,
            }
        })
        .collect();
    let mut s = shown.join(", ");
    if paths.len() > 5 {
        s.push_str(&format!(" (+{} more)", paths.len() - 5));
    }
    s
}

/// Extra self-report-vs-reality checks folded into the footer
/// (docs/feedback-2026-09-15 P1/P2, docs/feedback-2026-09-17 P2-2). All I/O
/// (git blobs, working-tree reads, this run's own step trace) happens in
/// `run_core`; this is just the already-decided result, so `footer_bits`
/// stays pure and its formatting stays unit-tested without a real repo or a
/// real transcript on disk.
#[derive(Default)]
struct FooterNotes {
    /// Per-`artifacts`-entry `wrote:` line-count note, aligned by index with
    /// the `artifacts` slice passed to `footer_bits`/`with_footer`.
    line_notes: Vec<Option<String>>,
    /// `"<short path> CRLF→LF"` entries for `artifacts` paths whose dominant
    /// line ending flipped between HEAD and the working tree.
    eol_changed: Vec<String>,
    /// Already-formatted `⚠ ran "..." N times` text when one normalized
    /// `run_command` line repeated at least `agy::REPEATED_COMMAND_THRESHOLD`
    /// times in this run's own step trace.
    repeated_command: Option<String>,
    /// Short-pathed, left-behind-looking new files (`*.old`, `*.tmp`, …)
    /// attributable to this run specifically — see `stray_artifacts`.
    stray_artifacts: Vec<String>,
    /// Short-pathed `artifacts` entries outside the run's `scope` — see
    /// `outside_scope`. Rendered as `⚠ outside scope: …` right after `wrote:`.
    outside_scope: Vec<String>,
    /// The preset's `expect_files` that do not exist after the run — see
    /// `missing_expected_files`. Rendered as `✗ expected file missing: …` just
    /// before the verify line.
    missing_expected: Vec<String>,
}

/// Build the token-frugal sub-agent footer line (`↳ N steps · wrote: … · also
/// changed during run (no write receipt): … · other jobs ran in this workspace
/// meanwhile: … · ~Nk ctx · agy_diag(trace) for detail`), or None if there's
/// nothing worth appending.
///
/// `wrote:` is the transcript's OWN write receipts (`artifacts`) only — never the
/// git diff. `also_changed` is the separate git-only remainder (paths git saw
/// dirty with no matching receipt): folding those into `wrote:` used to make a
/// job report files it never touched whenever a sibling job (or the supervising
/// session) edited the same tree concurrently — real case: a job told not to
/// touch `*.md` reported `wrote:` full of two sibling jobs' markdown edits.
/// `overlapping_job_ids` names who else ran here at the same time, when the disk
/// job mirror knows, so the reader isn't left guessing whether `also_changed` is
/// concurrency noise or this run's own shell-made edit agy's receipts missed.
///
/// `notes` (`FooterNotes`) folds in the mechanical self-report-vs-reality
/// checks from docs/feedback-2026-09-15/-17: a per-`wrote:`-path line-count
/// delta and EOL flip, a same-command-repeated-N-times warning, and stray
/// left-behind files (`*.old`/`*.tmp`/…) attributable to this run. All of it
/// arrives pre-computed (I/O happened in `run_core`) so this stays pure.
#[allow(clippy::too_many_arguments)]
fn footer_bits(
    step_summary: &str,
    artifacts: &[String],
    also_changed: &[String],
    overlapping_job_ids: &[String],
    git_available: bool,
    ctx_tokens: u64,
    ws: &Path,
    via: &str,
    verify: Option<&str>,
    notes: &FooterNotes,
) -> Option<String> {
    let mut bits: Vec<String> = Vec::new();
    if !via.is_empty() {
        bits.push(format!("answer: {via}"));
    }
    if !step_summary.is_empty() {
        // "6 steps: 3×run_command, ..." -> "6 steps"
        bits.push(
            step_summary
                .split(':')
                .next()
                .unwrap_or(step_summary)
                .trim()
                .to_string(),
        );
    }
    if let Some(w) = &notes.repeated_command {
        bits.push(w.clone());
    }
    // Without git, the transcript's receipts are the ONLY source for "what did
    // this run write" — say so, rather than letting a silent `wrote:` (or its
    // total absence) read as "verified against the tree" when there is no tree
    // to check it against.
    match (artifacts.is_empty(), git_available) {
        (false, true) => bits.push(format!(
            "wrote: {}",
            fmt_paths_capped_with_notes(artifacts, ws, &notes.line_notes)
        )),
        (false, false) => bits.push(format!(
            "wrote: {} (no git — receipts only)",
            fmt_paths_capped_with_notes(artifacts, ws, &notes.line_notes)
        )),
        (true, false) => bits.push("(no git — receipts only)".to_string()),
        (true, true) => {}
    }
    if !notes.eol_changed.is_empty() {
        bits.push(format!("⚠ eol changed: {}", notes.eol_changed.join(", ")));
    }
    if !notes.outside_scope.is_empty() {
        bits.push(format!(
            "⚠ outside scope: {}",
            notes.outside_scope.join(", ")
        ));
    }
    if !also_changed.is_empty() {
        bits.push(format!(
            "also changed during run (no write receipt): {}",
            fmt_paths_capped(also_changed, ws)
        ));
        if !overlapping_job_ids.is_empty() {
            bits.push(format!(
                "other jobs ran in this workspace meanwhile: {}",
                overlapping_job_ids.join(", ")
            ));
        }
    }
    if !notes.stray_artifacts.is_empty() {
        bits.push(format!(
            "⚠ stray artifacts left in workspace: {}",
            notes.stray_artifacts.join(", ")
        ));
    }
    if !notes.missing_expected.is_empty() {
        bits.push(format!(
            "✗ expected file missing: {}",
            notes.missing_expected.join(", ")
        ));
    }
    if let Some(v) = verify {
        bits.push(v.to_string());
    }
    if ctx_tokens > 0 {
        bits.push(format!("~{} ctx", agy::fmt_tok(ctx_tokens)));
    }
    // The trace hint is only useful when there were actual steps/artifacts.
    if !step_summary.is_empty() || !artifacts.is_empty() {
        bits.push("`agy_diag(trace)` for detail".to_string());
    }
    if bits.is_empty() {
        None
    } else {
        Some(format!("↳ {}", bits.join(" · ")))
    }
}

/// Cap a sub-agent answer so a huge reply can't flood the orchestrator's context.
/// Over `limit` chars: spill the full text to `<ws>/.agy-answer-<id>.md` and return
/// a head + pointer (lossless — the agent reads the file if it needs the rest).
/// `limit == 0` disables.
fn cap_answer(text: String, ws: &Path, conv_id: Option<&str>, limit: usize) -> String {
    if limit == 0 || text.chars().count() <= limit {
        return text;
    }
    let short = conv_id
        .and_then(|c| c.split('-').next())
        .filter(|s| !s.is_empty())
        .unwrap_or("latest");
    let path = ws.join(format!(".agy-answer-{short}.md"));
    let head: String = text.chars().take(limit).collect();
    match std::fs::write(&path, &text) {
        Ok(_) => format!(
            "{head}\n\n…[answer truncated to {limit} chars; full answer written to {}]",
            path.display()
        ),
        Err(e) => format!(
            "{head}\n\n…[answer truncated to {limit} chars; could not write the full answer to a file: {e}]"
        ),
    }
}

/// Append the sub-agent footer to an answer (step count, written files, the
/// resulting context size, and a trace hint). Minimal for trivial/chat answers.
#[allow(clippy::too_many_arguments)]
fn with_footer(
    text: String,
    step_summary: &str,
    artifacts: &[String],
    also_changed: &[String],
    overlapping_job_ids: &[String],
    git_available: bool,
    ctx_tokens: u64,
    ws: &Path,
    via: &str,
    verify: Option<&str>,
    notes: &FooterNotes,
) -> String {
    match footer_bits(
        step_summary,
        artifacts,
        also_changed,
        overlapping_job_ids,
        git_available,
        ctx_tokens,
        ws,
        via,
        verify,
        notes,
    ) {
        Some(f) => format!("{text}\n\n{f}"),
        None => text,
    }
}

/// Whether a preset's verify check should execute given the workspace's git diff.
/// Without the empty-slice skip, read-only runs in a repo waste time and quota
/// running builds on unchanged code; without running on `None`, non-git workspaces
/// would silently bypass preset verification entirely.
fn should_run_verify(git_changed: Option<&[String]>) -> bool {
    !matches!(git_changed, Some(c) if c.is_empty())
}

/// Normalize a path string for cross-source comparison: separators to `/`, and
/// case-folded on Windows (NTFS is case-insensitive, so a transcript receipt and
/// a git path for the same file can differ only in case). Pure so the matching
/// used to split `wrote:` from "also changed" is unit-tested without a real repo.
fn norm_path_for_match(p: &str) -> String {
    let s = p.replace('\\', "/");
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// Git-diff paths (workspace-relative) that have no matching transcript receipt
/// (`artifacts`, absolute). This is what a run's `wrote:` must NOT claim: the
/// transcript is the sub-agent's own account of what IT wrote, and a path git
/// saw dirty that never appears there was touched by something else — a sibling
/// job, the supervising session, or a shell command outside agy's own write
/// tool. Real case: a job told not to touch `*.md` reported `wrote:` full of
/// markdown files two other jobs had edited in the same tree.
fn git_only_paths(git_changed: &[String], artifacts: &[String], ws: &Path) -> Vec<String> {
    // Absolute (workspace-joined), like the receipts: `fmt_paths_capped` /
    // `short_path` already know how to shorten an absolute path against `ws`,
    // and a caller comparing this list against `artifacts` elsewhere gets the
    // same representation on both sides for free.
    git_changed
        .iter()
        .filter_map(|p| {
            let abs = ws.join(p).to_string_lossy().into_owned();
            let normed = norm_path_for_match(&abs);
            (!artifacts.iter().any(|a| norm_path_for_match(a) == normed)).then_some(abs)
        })
        .collect()
}

/// Best-effort workspace-relative, forward-slashed path for a `wrote:` note —
/// None when `abs` isn't actually under `ws` (e.g. an `add_dirs` write),
/// since there's no HEAD blob to compare a path outside the repo against.
/// Pure string manipulation, reusing `norm_path_for_match`'s separator/case
/// folding to decide the boundary so this agrees with `git_only_paths` on
/// what counts as "the same path" — but slices the ORIGINAL (non-lowercased)
/// string for the result, since the returned path is fed to `git show` and a
/// case-sensitive lookup should see the real name.
fn rel_under_workspace(ws: &Path, abs: &str) -> Option<String> {
    let ws_n = norm_path_for_match(&ws.to_string_lossy());
    let abs_n = norm_path_for_match(abs);
    if !abs_n.starts_with(&ws_n) {
        return None;
    }
    let slashed = abs.replace('\\', "/");
    // `norm_path_for_match` only lowercases/swaps separators, never changes
    // byte length for the ASCII paths this deals with in practice, so slicing
    // `slashed` at `ws_n`'s length lines up with the same boundary.
    let rel = slashed.get(ws_n.len()..)?.trim_start_matches('/');
    if rel.is_empty() {
        None
    } else {
        Some(rel.to_string())
    }
}

/// Per-`artifacts`-entry `wrote:` notes (line-count delta + EOL flip) —
/// docs/feedback-2026-09-15 P1. One `git show`/file-read pair per artifact
/// path (`agy::wrote_path_note`), so this only runs on the (usually small)
/// receipt list, not the whole diff. A path outside the workspace (e.g. an
/// `add_dirs` write) has no HEAD blob to compare against and gets no note.
fn gather_wrote_notes(ws: &Path, artifacts: &[String]) -> (Vec<Option<String>>, Vec<String>) {
    let mut line_notes = Vec::with_capacity(artifacts.len());
    let mut eol_entries = Vec::new();
    let rels: Vec<String> = artifacts
        .iter()
        .filter_map(|p| rel_under_workspace(ws, p))
        .collect();
    let normalized = agy::git_normalized_paths(ws, &rels);
    for p in artifacts {
        match rel_under_workspace(ws, p) {
            Some(rel) => {
                let note = agy::wrote_path_note(ws, &rel);
                if let Some((before, after)) =
                    note.eol_change.filter(|_| !normalized.contains(&rel))
                {
                    eol_entries.push(format!("{} {before}→{after}", short_path(p, ws)));
                }
                line_notes.push(note.line_count);
            }
            None => line_notes.push(None),
        }
    }
    (line_notes, eol_entries)
}

/// Whether a path looks like a stray artifact a run left behind — a
/// backup/temp/report file created via a shell command rather than the
/// agent's own write tool (docs/feedback-2026-09-15 P2: a 163KB `.old`
/// backup and a scratch `.tmp` survived in the repo after a run), a
/// `tmp_*` scratch script, or Python bytecode (gen3dasset 2026-09-22: a
/// throwaway script and its `__pycache__` were left in the repo). Path text
/// only, no I/O.
fn looks_stray(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    is_python_bytecode(path)
        || name.starts_with("tmp_")
        || name.ends_with(".old")
        || name.ends_with(".bak")
        || name.ends_with(".orig")
        || name.ends_with(".tmp")
        || name.ends_with('~')
        || name.ends_with("_REPORT.md")
        || name.ends_with("_report.md")
}

/// Absolute on either OS's terms (`/x`, `\x`, `C:…`), so tests read the same everywhere.
fn is_abs_path(s: &str) -> bool {
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

/// Path components with `/` separators, lowercased on Windows, `.` and empties dropped,
/// `..` resolved lexically (so `src/../Cargo.toml` is not "under src") and a verbatim
/// `\\?\` prefix stripped (so `\\?\C:\x` matches `C:\x`).
fn norm_components(p: &Path) -> Vec<String> {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).replace('\\', "/");
    let s = if cfg!(windows) { s.to_lowercase() } else { s };
    let mut out: Vec<String> = Vec::new();
    for seg in s.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            _ => out.push(seg.to_string()),
        }
    }
    out
}

/// Does `path_comps` start with all of `prefix_comps`? An empty prefix matches nothing.
fn path_has_prefix(path_comps: &[String], prefix_comps: &[String]) -> bool {
    if prefix_comps.is_empty() {
        return false;
    }
    if path_comps.len() < prefix_comps.len() {
        return false;
    }
    &path_comps[..prefix_comps.len()] == prefix_comps
}

/// The entries of `expected` that do not exist: a relative entry is joined onto
/// `ws`; returned as given, order kept. Pure over `exists`.
fn missing_expected_files(
    expected: &[String],
    ws: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Vec<String> {
    let mut out = Vec::new();
    for entry in expected {
        let p = if is_abs_path(entry) {
            PathBuf::from(entry)
        } else {
            ws.join(entry)
        };
        if !exists(&p) {
            out.push(entry.clone());
        }
    }
    out
}

/// The write receipts (`artifacts`) that fall outside `scope`. Pure.
///
/// Only receipts are judged: they are this run's own writes, while the git-only
/// `also_changed` may be a sibling job's (see `footer_bits`). A scope entry is a
/// path prefix matched by whole components (`src/core` covers `src/core/a.rs`,
/// not `src/core2/b.rs`); a relative entry or artifact is joined onto `ws`.
/// Separators and, on Windows, case are normalized before comparing. `reports`
/// (the bridge's own findings dir) is always in scope. Order of `artifacts` is
/// kept; the returned paths are as given in `artifacts`.
fn outside_scope(artifacts: &[String], ws: &Path, scope: &[String], reports: &Path) -> Vec<String> {
    let reports_abs = if is_abs_path(&reports.to_string_lossy()) {
        reports.to_path_buf()
    } else {
        ws.join(reports)
    };
    let reports_comps = norm_components(&reports_abs);

    let scope_comps_list: Vec<Vec<String>> = scope
        .iter()
        .map(|s| {
            let p = if is_abs_path(s) {
                PathBuf::from(s)
            } else {
                ws.join(s)
            };
            norm_components(&p)
        })
        .collect();

    let mut out = Vec::new();
    for art in artifacts {
        let art_p = if is_abs_path(art) {
            PathBuf::from(art)
        } else {
            ws.join(art)
        };
        let art_comps = norm_components(&art_p);

        let in_reports = path_has_prefix(&art_comps, &reports_comps);
        let in_scope = scope_comps_list
            .iter()
            .any(|sc| path_has_prefix(&art_comps, sc));

        if !in_reports && !in_scope {
            out.push(art.clone());
        }
    }
    out
}

/// Stray-artifact footer candidates: which of `git_only` (this run's
/// unattributed git-diff paths — see `git_only_paths`) match a left-behind
/// pattern (`looks_stray`) AND are attributable to THIS run specifically,
/// because its path text appears in one of this run's own `run_command`
/// lines (the shell command that plausibly created it, e.g. `git show
/// HEAD:x > x.old`). Also checks `artifacts` (this run's own write receipts)
/// directly, in case a stray-shaped file was written via the agent's own
/// write tool instead. A stray-shaped path that matches neither test is left
/// alone — it stays wherever it already was (`also_changed`, most likely,
/// since a path already in `artifacts` never appears in `git_only` at all),
/// because attributing an unexplained one to this run would be a guess
/// (docs/feedback-2026-09-15 P2, docs/feedback-2026-09-17 P2-2).
///
/// Bytecode is the one exception to "the command names the path": no command
/// ever names a `__pycache__` entry, so it is pinned on this run when one of
/// its commands ran Python.
fn stray_artifacts(
    git_only: &[String],
    artifacts: &[String],
    run_commands: &[String],
) -> Vec<String> {
    let ran_python = run_commands
        .iter()
        .any(|c| c.contains("python") || c.contains("pytest") || c.contains("py -"));
    let mut out: Vec<String> = artifacts
        .iter()
        .filter(|p| looks_stray(p))
        .cloned()
        .collect();
    out.extend(
        git_only
            .iter()
            .filter(|p| {
                looks_stray(p)
                    && (run_commands.iter().any(|c| c.contains(p.as_str()))
                        || (ran_python && is_python_bytecode(p)))
            })
            .cloned(),
    );
    out.sort();
    out.dedup();
    out
}

/// A `__pycache__` entry or a `.pyc` — written by running Python, never asked for.
fn is_python_bytecode(path: &str) -> bool {
    path.ends_with(".pyc") || path.split(['/', '\\']).any(|c| c == "__pycache__")
}

/// What proves a `preset=code` run that left no write receipts actually changed
/// nothing — feeding the punt nudge (e.g. a run that only added a dependency and
/// stopped short of the implementation).
enum PuntEvidence {
    /// Not a git workspace: the transcript's empty artifact list is all there is.
    NoGit,
    /// Git saw an empty diff: proof, not just an absence of claims.
    CleanGitTree,
    /// Git saw a diff, but every path in it falls inside another job's run
    /// window in the same workspace — it can't be pinned on this run either.
    OverlappingJobsOnly,
}

/// Decide the punt evidence, or None to stay silent. Only reached with NO write
/// receipts (`has_receipts=false`) — a receipt is proof of work, so nothing here
/// gets consulted otherwise. Deliberately does NOT fire when there are no
/// receipts but an unattributed git diff exists with no overlapping job: the run
/// most likely worked through shell commands agy's own transcript doesn't
/// receipt, and calling that a punt would be a false alarm.
fn punt_evidence(
    has_receipts: bool,
    git_changed: Option<&[String]>,
    overlapping_job_ids: &[String],
) -> Option<PuntEvidence> {
    if has_receipts {
        return None;
    }
    match git_changed {
        None => Some(PuntEvidence::NoGit),
        Some([]) => Some(PuntEvidence::CleanGitTree),
        Some(_) if !overlapping_job_ids.is_empty() => Some(PuntEvidence::OverlappingJobsOnly),
        Some(_) => None,
    }
}

/// The punt-nudge text prepended to `text`, given the reason (`how`, from
/// `punt_evidence`'s branches). Split out of `run_core` as a pure function so
/// its wording — including the read-only-task escape hatch below — is
/// unit-tested directly rather than only by re-typing the literal in a test.
///
/// docs/feedback-2026-09-17 P4: a read-only investigation/verification job
/// (nothing to write, by design) tripped this same nudge, which reads as "the
/// task failed" when it didn't. The nudge still fires — there's no cheap way
/// here to tell "investigation, by design" from "punted, by accident" — but
/// says a read-only task can ignore it, and names the preset that marks
/// intent up front instead of leaving it to be reverse-engineered after the
/// fact.
fn punt_nudge_text(how: &str, text: &str) -> String {
    format!(
        "⚠ this task reported done but {how} — it may have punted (e.g. only \
         added a dependency). Verify before trusting it. If this was actually a \
         read-only investigation/verification task, this warning can be ignored — \
         dispatch that kind of work with preset=research next time.\n\n{text}"
    )
}

/// The disk-mirrored half of [`agy::overlapping_jobs`]: read the shared job
/// records (other agy-mcp processes' jobs only exist there) and format the
/// result, or None when nothing else ran here. Not itself a decision — the
/// decision is `agy::overlapping_jobs`, tested on its own — this is only the I/O
/// wrapper, shared by the success footer and the failure path's `changed on
/// disk:` context so the two can't word it differently.
fn workspace_concurrency_note(this_job_id: &str, ws: &str, start: u64, end: u64) -> Option<String> {
    let records = agy::list_job_records();
    let ids = agy::overlapping_jobs(this_job_id, ws, start, end, &records);
    (!ids.is_empty()).then(|| {
        format!(
            "other jobs ran in this workspace meanwhile: {}",
            ids.join(", ")
        )
    })
}

/// The disk half of [`agy::shared_tree_note`]: the job records (other servers'
/// jobs only exist there) and, per running sibling, the paths its transcript has
/// receipted so far. A transcript that can't be read yet just shows no writes.
fn shared_tree_note_for(this_job_id: &str, ws: &Path) -> Option<String> {
    let records = agy::list_job_records();
    let ws_str = ws.to_string_lossy();
    let siblings: Vec<(&agy::JobRecord, Vec<String>)> =
        agy::running_in_workspace(this_job_id, &ws_str, &records, &is_pid_alive)
            .into_iter()
            .map(|r| {
                let paths = r
                    .conversation_id
                    .as_deref()
                    .and_then(agy::resolve_conv_dir)
                    .and_then(|dir| agy::read_full(&dir).ok())
                    .map(|t| t.artifacts.iter().map(|p| short_path(p, ws)).collect())
                    .unwrap_or_default();
                (r, paths)
            })
            .collect();
    agy::shared_tree_note(&siblings)
}

#[derive(Clone, Copy)]
enum JobStatus {
    Running,
    Done,
    Failed,
}

/// A background (`async`) run, tracked in-memory for agy_poll.
struct Job {
    status: JobStatus,
    tool: &'static str,
    model: String,
    started: SystemTime,
    /// When the job reached a terminal state (Done/Failed); None while running.
    /// Used so the reported duration is the real run time, not time-since-start
    /// (which would keep growing the later you poll a finished job).
    finished: Option<SystemTime>,
    prompt_head: String,
    /// The final footer text (Done) or the error message (Failed).
    result: Option<String>,
    /// Whether the caller has actually retrieved this job's terminal result via
    /// agy_poll. A job that finished but was never collected (the caller fired
    /// async and lost track — e.g. forgot to run `agy-mcp wait`, or the session
    /// got interrupted first) stays `false` so [`AgyServer::pending_jobs_banner`]
    /// keeps surfacing it instead of letting the result silently rot unseen.
    seen: bool,
}
impl Job {
    /// Run duration: finished − started once terminal, else elapsed-so-far.
    fn secs(&self) -> u64 {
        match self.finished {
            Some(f) => f
                .duration_since(self.started)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            None => self.started.elapsed().map(|d| d.as_secs()).unwrap_or(0),
        }
    }
}

/// Select finished jobs to evict to bring total jobs down by `over_by`.
/// Prefers finished jobs with `seen == true` first (oldest-first), and only falls
/// back to `seen == false` finished jobs (oldest-first) if needed.
fn jobs_to_evict(jobs: &HashMap<String, Job>, over_by: usize) -> Vec<String> {
    if over_by == 0 {
        return Vec::new();
    }
    let mut seen_finished: Vec<u64> = Vec::new();
    let mut unseen_finished: Vec<u64> = Vec::new();

    for (k, j) in jobs {
        if !matches!(j.status, JobStatus::Running) {
            if let Some(n) = k.strip_prefix('j').and_then(|s| s.parse::<u64>().ok()) {
                if j.seen {
                    seen_finished.push(n);
                } else {
                    unseen_finished.push(n);
                }
            }
        }
    }

    seen_finished.sort_unstable();
    unseen_finished.sort_unstable();

    let mut evicted = Vec::new();
    for n in seen_finished {
        if evicted.len() >= over_by {
            break;
        }
        evicted.push(format!("j{n}"));
    }
    if evicted.len() < over_by {
        for n in unseen_finished {
            if evicted.len() >= over_by {
                break;
            }
            evicted.push(format!("j{n}"));
        }
    }
    evicted
}

#[derive(Default)]
struct State {
    /// Most recent conversation id (for agy_sub resume / agy_trace).
    last_conv_id: Option<std::sync::Arc<String>>,
    /// Background jobs by id.
    jobs: HashMap<String, Job>,
    last_job: Option<String>,
    /// The connected client, captured at `initialize`. Kept so the server can ask
    /// it for its roots (MCP `roots/list`) — that's the only sanctioned way to
    /// learn the user's project dir without guessing from cwd (invariant #5).
    peer: Option<Peer<RoleServer>>,
    /// Whether that client advertised the `roots` capability. `None` until
    /// `initialize` runs. Recorded so `agy_diag(status)` can say why `workspace`
    /// still has to be passed by hand on a client that doesn't offer roots.
    client_roots: Option<bool>,
    /// Client name/version, for the same diagnostic.
    client_name: Option<String>,
}

/// Parse a `file://` root URI into a local path. MCP roots are URIs; Windows ones
/// arrive as `file:///C:/x/y`, so the leading slash has to come off before the
/// drive letter. Anything that isn't a `file://` URI is not a workspace.
fn root_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///C:/x` (Windows) vs `file:///home/x` (POSIX): strip the leading
    // slash only when a drive letter follows it.
    let path = match rest.strip_prefix('/') {
        Some(after) if after.as_bytes().get(1) == Some(&b':') => after,
        _ => rest,
    };
    let decoded = path.replace("%20", " ");
    if decoded.is_empty() {
        None
    } else {
        Some(PathBuf::from(decoded))
    }
}

/// The `client:` line of `agy_diag(status)`. Pure.
///
/// It used to report the `roots` **capability** the client advertised at
/// `initialize`, which is not the same question as "can I omit `workspace`?".
/// Measured 2026-08-15 on `local-agent-mode-agy 1.0.0`: status printed
/// `MCP roots: yes (workspace optional)` while `resolve_workspace(None)`, called
/// in the very same handler for the preset list, had already failed — `roots/list`
/// returned nothing usable. **The next session on the same client resolved fine**,
/// which is the actual finding: the answer varies per session, so a capability
/// flag captured once at `initialize` can never stand in for it. Report the
/// resolution itself — it folds in roots, `AGY_WORKSPACE` and the failure reason,
/// and it is the exact call `agy_sub` will make.
fn workspace_status_line(client: Option<&str>, resolved: Result<&Path, &str>) -> String {
    let Some(who) = client else {
        return "\nclient: not yet initialized".to_string();
    };
    match resolved {
        Ok(p) => format!(
            "\nclient: {who} · workspace: {} (arg optional)",
            p.display()
        ),
        // The resolver's own message names which of the two ways it failed
        // (no usable roots vs. several), so don't paraphrase it here.
        Err(e) => format!("\nclient: {who} · workspace: REQUIRED — {e}"),
    }
}

/// Everything needed to resume or re-dispatch a run WITHOUT the supervisor's
/// context. Persisted with the job record the moment the job is registered, so a
/// job that dies with its session (restart, crash, lost tool connection) costs
/// nothing to recover — the prompt is the expensive part, and it's on disk.
struct DispatchSpec {
    tool: &'static str,
    /// Display label for the model (what agy_poll shows).
    model: String,
    /// The prompt as dispatched (preset preamble included).
    prompt: String,
    /// 60-char head of the CALLER'S OWN prompt, taken before the preset preamble
    /// and workspace directive are prepended. `prompt` above always starts with
    /// the same ~generic preamble text, so a head cut from it can't tell one job
    /// apart from another — every running-job listing showed the identical
    /// "You are an autonomous coding agent with file and shell acces" (observed).
    prompt_head: String,
    workspace: String,
    add_dirs: Vec<String>,
    preset: String,
    /// Set when this run resumes an existing conversation; a fresh run records
    /// its conversation id mid-flight instead (see `RunOpts::job_id`).
    conversation_id: Option<String>,
}

/// One run's inputs to [`AgyServer::run_core`]. A struct rather than a parameter
/// A workspace that does not exist, refused before agy is spawned: the spawn fails
/// on it with os error 267 (bad current directory), which reads as "agy is not
/// installed" and sends the caller to set AGY_BIN.
fn workspace_missing_message(ws: &std::path::Path) -> Option<String> {
    (!ws.is_dir()).then(|| format!("workspace does not exist: {}", ws.display()))
}

/// Presets whose answer is only worth having from the model that was asked for.
fn strict_model_preset(name: &str) -> bool {
    name == "review"
}

/// The failure text for a strict preset that was served by another model.
fn strict_model_message(req: &str, got: &str, conv_id: Option<&str>) -> String {
    let conv = conv_id
        .map(|c| format!(" The reply is kept in conversation {c} (agy_diag trace)."))
        .unwrap_or_default();
    format!(
        "review needs \"{req}\" but agy served \"{got}\" — its picker fell back to a different model, so this is not the second opinion you asked for. Retry later or name another model.{conv}"
    )
}

/// list because the list had grown to a dozen, several of them same-typed and
/// adjacent (`Option<String>` / `bool` / `u64`) where a transposition compiles
/// cleanly and misbehaves at runtime. Mirrors the shape of [`agy::RunOpts`].
struct RunCoreOpts {
    prompt: String,
    model: Option<String>,
    workspace: PathBuf,
    add_dirs: Vec<String>,
    continue_id: Option<String>,
    mcp: agy::McpMode,
    timeout_s: u64,
    max_answer_chars: usize,
    tool: &'static str,
    /// The preset promised this run would change files — arms the punt nudge.
    expects_changes: bool,
    /// A served model on another slot than requested is a failure, not a ⚠ (preset
    /// `review`: a second opinion from the wrong model is worth nothing).
    strict_model: bool,
    /// The preset's supervisor-side check, already vetted at resolve time. None = off.
    verify: Option<String>,
    /// Where to report "still working" while the run is in flight (the caller's
    /// MCP progress token, if it asked for one). None = it didn't.
    progress: Option<agy::ProgressSink>,
    /// Non-blocking advisory prepended to the answer (the ~200k ctx warning).
    ctx_warn: Option<String>,
    /// Mid-run ctx watchdog ceiling; None = uncapped for this model.
    ctx_limit: Option<u64>,
    /// The preset's guard policy for this run, as JSON, handed to the PreToolUse
    /// guard through agy's environment. None = the layered policy files decide.
    guard_policy: Option<String>,
    /// The preset's `env` table, already `{workspace}`/`{reports}`-substituted
    /// (`presets::substituted_env`). Handed to both the agy child process and its
    /// `verify` command; empty when the preset sets none.
    preset_env: std::collections::BTreeMap<String, String>,
    agent: Option<String>,
    /// Paths this run may write, placeholders already substituted: the
    /// `agy_sub(scope=…)` argument if given, else the preset's. None = unchecked.
    scope: Option<Vec<String>>,
    /// Files the run must leave behind, placeholders already substituted (preset
    /// only). None = unchecked.
    expect_files: Option<Vec<String>>,
    job_id: String,
}

#[derive(Clone)]
struct AgyServer {
    tool_router: ToolRouter<AgyServer>,
    /// Bounds concurrent agy runs. Every run takes 1 permit, `agy_diag(live)`
    /// included; nothing runs exclusively.
    concurrency: Arc<Semaphore>,
    /// Held only across spawn → new-conversation-dir detection for FRESH runs, so
    /// concurrent fresh launches each capture their own dir (resumes don't need it).
    spawn_gate: Arc<Mutex<()>>,
    /// Guards the shared `mcp_config.json` read window (see `RunOpts::swap_gate`).
    /// A custom-mcp run used to take every permit for its whole duration; it now
    /// takes the write side of this for a few seconds around spawn instead, which
    /// is the interval that actually needed protecting.
    swap_gate: Arc<tokio::sync::RwLock<()>>,
    /// Fast, never held across an agy run: last conv id + background jobs.
    state: Arc<Mutex<State>>,
}

#[tool_router]
impl AgyServer {
    fn new() -> Self {
        let max = std::env::var("AGY_MAX_CONCURRENT")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|n| *n >= 1)
            .unwrap_or(10);
        let tool_router = Self::tool_router();
        Self {
            tool_router,
            concurrency: Arc::new(Semaphore::new(max as usize)),
            spawn_gate: Arc::new(Mutex::new(())),
            swap_gate: Arc::new(tokio::sync::RwLock::new(())),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Name the client when `initialize` didn't. A 2026-07-28 client (Antigravity) opens
    /// with `server/discover` and never initializes; it names itself in each request's
    /// `_meta` instead. `inline_wait_s` and the status line read the name.
    async fn note_client(&self, ctx: &RequestContext<RoleServer>) {
        let mut st = self.state.lock().await;
        if st.client_name.is_none() {
            if let Some(info) = ctx.client_info() {
                st.client_name = Some(format!("{} {}", info.name, info.version));
            }
        }
    }

    /// Acquire the one concurrency permit a run holds.
    async fn acquire(&self) -> OwnedSemaphorePermit {
        self.concurrency
            .clone()
            .acquire_owned()
            .await
            .expect("concurrency semaphore is never closed")
    }

    /// Refresh the quota cache in the background when it has gone stale.
    ///
    /// "When should I check quota?" was a decision the caller had to remember to
    /// make, and the honest answer — after a run, before the next one — is exactly
    /// when the server can see for itself. It takes one permit with `try_`: when
    /// every slot is busy the refresh is skipped, never queued, so it can't ever
    /// delay a real dispatch.
    ///
    /// `force` skips only the staleness check: a run that just hit a quota wall wrote
    /// a fresh 0% with no refresh time, and `/usage` has one.
    fn maybe_refresh_quota(&self, force: bool) {
        if !force && !agy::quota_needs_refresh() {
            return;
        }
        let Ok(permit) = self.concurrency.clone().try_acquire_owned() else {
            return; // every slot busy — try again after the next run
        };
        tokio::spawn(async move {
            let _permit = permit;
            let _ = agy::live_quota().await; // logs the snapshot as a side effect
        });
    }

    /// Core ask path: bound concurrency (acquire), run agy, record the conv id, and
    /// format the report + footer. Owned fields so it can also drive a detached
    /// async job. Returns anyhow::Error so the async path can store the message.
    async fn run_core(&self, o: RunCoreOpts) -> anyhow::Result<String> {
        let RunCoreOpts {
            prompt,
            model,
            workspace,
            add_dirs,
            continue_id,
            mcp,
            timeout_s,
            max_answer_chars,
            tool,
            expects_changes,
            strict_model,
            verify,
            progress,
            ctx_warn,
            ctx_limit,
            guard_policy,
            preset_env,
            agent,
            scope,
            expect_files,
            job_id,
        } = o;
        let _permit = self.acquire().await; // bound concurrency
                                            // Make the passed workspace authoritative. agy's tool-runner otherwise
                                            // defaults to its own scratch even with cwd set; the absolute-path directive
                                            // + the `--add-dir <workspace>` in run_agy land files in the workspace.
        let prompt = prepend_workspace_directive(&workspace, prompt);
        // Ground truth for "what did this run write". The transcript's success
        // receipts are the sub-agent's own account of itself; the working tree is
        // not. Snapshot before/after and diff. None = not a git work tree, in which
        // case the transcript stays the only source (an empty diff from a non-repo
        // must never read as "changed nothing").
        let git_before = agy::git_snapshot(&workspace);
        // This run's own wall-clock window, for attributing a git-only change to a
        // concurrent sibling job rather than to this run (see `workspace_concurrency_note`).
        let run_start_unix = unix(SystemTime::now());
        let ws_str = workspace.to_string_lossy().into_owned();
        let mode_is_inherit = matches!(mcp, agy::McpMode::Inherit);
        let t0 = std::time::Instant::now();
        let res = agy::run_agy(RunOpts {
            prompt: &prompt,
            workspace: &workspace,
            model: model.as_deref(),
            add_dirs: &add_dirs,
            continue_id: continue_id.as_deref(),
            timeout_s,
            idle_timeout_s: default_idle_timeout(),
            mcp,
            spawn_gate: Some(&self.spawn_gate),
            swap_gate: Some(&self.swap_gate),
            job_id: Some(&job_id),
            progress: progress.clone(),
            guard_policy: guard_policy.clone(),
            preset_env: &preset_env,
            agent: agent.as_deref(),
        })
        .await;
        let elapsed = t0.elapsed().as_secs_f64();
        let mut r = match res {
            Ok(r) => r,
            Err(e) => {
                // The progress note names the steps a failed run took; without it the
                // run log recorded every cut run as `steps: 0`.
                let err_text = format!("{e:#}");
                let steps = err_text
                    .split("before the failure: ")
                    .nth(1)
                    .map(agy::steps_of)
                    .unwrap_or(0);
                agy::log_run(tool, model.as_deref(), None, steps, 0, elapsed, false);
                // A 429 receipt is a free, authoritative quota reading — record it
                // so the NEXT dispatch refuses up front instead of re-learning it.
                // A bare resume passes no `--model`, so `record_quota_wall` would
                // otherwise learn nothing from the receipt; fall back to the
                // conversation's own served model (`resolve_conv_dir`, not a plain
                // join, because `continue_id` may be the short prefix `agy_sessions`
                // shows rather than the full directory name).
                let conv_model = continue_id
                    .as_deref()
                    .and_then(agy::resolve_conv_dir)
                    .and_then(|dir| agy::read_conv_model(&dir));
                let learned = agy::record_quota_wall(
                    model.as_deref(),
                    conv_model.as_deref(),
                    &format!("{e:#}"),
                );
                // Don't also guess at a probable cause when we just recorded the
                // real one; the receipt is already in the error.
                let effective_model = model.as_deref().or(conv_model.as_deref());
                let mut e = match (learned, agy::quota_probable_cause(effective_model)) {
                    (false, Some(hint)) => e.context(hint),
                    _ => e,
                };
                if learned {
                    if let Some(group) =
                        agy::resolve_quota_group(model.as_deref(), conv_model.as_deref())
                    {
                        // Hours or days: the receipt can't say, a scraped reading can.
                        if let Some(note) = agy::wall_window_note(group) {
                            e = e.context(note);
                        }
                        if let Some(alt) = agy::cached_quota_alternative(group) {
                            e = e.context(alt);
                        }
                    }
                }
                // A cached slug the catalog has since dropped still *resolves* (the
                // resolve-miss refresh never fires), so agy is the first to notice.
                // Its refusal costs no quota; re-list so the very next call resolves
                // against the current catalog (seen 2026-10-03: claude-*-4-6 → 5-5).
                if agy::is_invalid_model_selection(&err_text) {
                    let changed = agy::refresh_models_cache().await;
                    e = e.context(if changed {
                        "the cached model list was stale and has been refreshed — \
                         retrying the same call resolves against the current catalog"
                    } else {
                        "agy rejected the model; the model list is already current"
                    });
                }
                // A failed run often still landed edits (invariant #9), and the
                // denial message can only tell the reader to go check git itself.
                // Name the files instead — the snapshot is already half-taken.
                if let Some(b) = &git_before {
                    if let Some(a) = agy::git_snapshot(&workspace) {
                        let diff = agy::git_changed_paths(b, &a);
                        if !diff.is_empty() {
                            let mut msg =
                                format!("changed on disk: {}", fmt_paths_capped(&diff, &workspace));
                            // The failure path has no transcript to compare against (agy
                            // never returned one), so none of `diff` can be pinned on
                            // this run either — same reasoning as the punt nudge's
                            // OverlappingJobsOnly branch, named here instead of left for
                            // the reader to rediscover via `agy-mcp jobs`.
                            let end = unix(SystemTime::now());
                            if let Some(note) =
                                workspace_concurrency_note(&job_id, &ws_str, run_start_unix, end)
                            {
                                msg.push_str(&format!(" · {note}"));
                            }
                            e = e.context(msg);
                        }
                    }
                }
                return Err(e);
            }
        };
        if let Some(id) = &r.conv_id {
            self.state.lock().await.last_conv_id = Some(std::sync::Arc::new(id.clone()));
        }
        // A backgrounded build/task landed after agy ended its turn, and the answer
        // is nothing but a "build is running…" waiting line — the model never saw
        // the outcome. Continue the conversation once so it writes the real report
        // (the completion is now in its history). Inherit MCP for this lightweight
        // summary turn (no swap).
        let should_resume = !r.task_results.is_empty() && agy::is_wait_only(&r.bare_answer);

        if should_resume {
            if let Some(cid) = r.conv_id.clone() {
                let nudge = "A background task you launched (e.g. a build) has now finished — \
                    its result is in the conversation above. Reply with a concise final summary: \
                    success or failure, and the key output. Do not start any new work."
                    .to_string();
                let r2 = agy::run_agy(RunOpts {
                    prompt: &nudge,
                    workspace: &workspace,
                    model: None, // keep the conversation's model
                    add_dirs: &add_dirs,
                    continue_id: Some(&cid),
                    timeout_s: timeout_s.min(300),
                    idle_timeout_s: default_idle_timeout(),
                    mcp: agy::McpMode::Inherit,
                    spawn_gate: Some(&self.spawn_gate),
                    swap_gate: Some(&self.swap_gate),
                    job_id: Some(&job_id),
                    progress: progress.clone(),
                    guard_policy: guard_policy.clone(),
                    preset_env: &preset_env,
                    agent: None,
                })
                .await;
                if let Ok(r2) = r2 {
                    if !r2.text.trim().is_empty() {
                        // model summary replaces the raw-result answer
                        r.text = format!("· auto-resumed for summary\n\n{}", r2.text.trim());
                        r.ctx_tokens = r2.ctx_tokens;
                    }
                }
            }
        } else if r.cut_off {
            // A partial answer with no final report — agy's own `--print-timeout`,
            // or the bridge's idle/hard-cap stop (never QuotaWall: that outcome is
            // now a failure before it ever reaches here, and resuming into the same
            // wall would just hit it again — see `agy::stop_forces_failure`). One
            // resume asking the sub-agent to stop and report beats handing the
            // caller a fragment with no account of what got done. `r.cut_off` comes
            // from THIS run's own outcome only, never the summary reply below, so
            // this can fire at most once. If the resume itself fails, `r` (and its
            // partial-answer text/warning prefix) is returned unchanged.
            if let Some(cid) = r.conv_id.clone() {
                let nudge = "The previous turn was cut off before you finished. Stop working now \
                    — do not start or continue any task. Reply with a concise summary: what you \
                    changed, what you verified, and what is still unfinished."
                    .to_string();
                let r2 = agy::run_agy(RunOpts {
                    prompt: &nudge,
                    workspace: &workspace,
                    model: None, // keep the conversation's model
                    add_dirs: &add_dirs,
                    continue_id: Some(&cid),
                    timeout_s: timeout_s.min(300),
                    idle_timeout_s: default_idle_timeout(),
                    mcp: agy::McpMode::Inherit,
                    spawn_gate: Some(&self.spawn_gate),
                    swap_gate: Some(&self.swap_gate),
                    job_id: Some(&job_id),
                    progress: progress.clone(),
                    guard_policy: guard_policy.clone(),
                    preset_env: &preset_env,
                    agent: None,
                })
                .await;
                if let Ok(r2) = r2 {
                    if !r2.text.trim().is_empty() {
                        r.text = format!(
                            "· auto-resumed for a summary after a cut-off run\n\n{}",
                            r2.text.trim()
                        );
                        r.ctx_tokens = r2.ctx_tokens;
                    }
                }
            }
        }
        agy::log_run(
            tool,
            model.as_deref(),
            r.served_model.as_deref(),
            agy::steps_of(&r.step_summary),
            r.ctx_tokens,
            elapsed,
            true,
        );
        tracing::info!(tool, via = r.via, model = ?r.served_model, conv = ?r.conv_id, "answered");
        let mut text = cap_answer(r.text, &workspace, r.conv_id.as_deref(), max_answer_chars);
        // Context notices, most specific first: a resume-time one (the session was
        // already big when we took it) wins over the after-the-fact one (this run
        // made it big), so a heavy resume never collects two paragraphs saying the
        // same thing.
        if let Some(warn) = ctx_warn.or_else(|| ctx_over_note(r.ctx_tokens, ctx_limit)) {
            text = format!("{warn}\n\n{text}");
        }
        // Surface skills dropped silently by agy's strict YAML parser so the caller knows they were skipped.
        let skill_roots = agy::skill_roots(Some(&workspace));
        let skills = agy::check_skills(&skill_roots);
        if let Some(warn) = agy::broken_skills_warning(&skills) {
            text = format!("{warn}\n\n{text}");
        }
        // Scoping MCP only modifies ~/.gemini/config/mcp_config.json; warn if workspace MCP servers stay on.
        let ws_servers = agy::workspace_mcp_servers(&workspace).unwrap_or_default();
        if let Some(note) = agy::mcp_scope_note(mode_is_inherit, &ws_servers) {
            text = format!("{note}\n\n{text}");
        }
        // Self-verify the served model against what we asked for. We pass --model as an
        // explicit request on every fresh run, resume-switch, and keep-model resume;
        // agy's picker can silently fall back to a DIFFERENT model — observed: an
        // unavailable slug (gemini-3.1-pro-high → served as Gemini 3.6 Flash), or a
        // resume that ignored --model and defaulted to Flash. If the served slot (last
        // receipt) differs from the requested slot, warn. Slot-level only: an effort/
        // version drift within the same family (agy over-reports those combos) is a
        // quality nuance, not a downgrade, and a per-run ⚠ there would be noise.
        if let Some(req) = model.as_deref() {
            let got = r.served_model.as_deref();
            if agy::slot_mismatch(req, got) {
                if strict_model {
                    return Err(anyhow::anyhow!(
                        "{}",
                        strict_model_message(req, got.unwrap_or("?"), r.conv_id.as_deref())
                    ));
                }
                text = format!(
                    "⚠ requested \"{req}\" but agy served \"{}\" — its picker silently fell back to \
                     a different model (an unavailable slug, or a resume that ignored --model). \
                     Retry, pick another model/effort, or run agy_diag(action=live, what=models) \
                     to refresh the slug list.\n\n{text}",
                    got.unwrap_or("?")
                );
            }
        }
        // The working tree's own account of the run, kept SEPARATE from the
        // transcript's artifact list rather than folded into it (until commit
        // f40ccf2 the transcript wasn't parsed at all, so git was the only source
        // and folding was harmless; now that receipts parse again, folding them
        // together made `wrote:` claim credit for a sibling job's or the
        // supervisor's concurrent edit to the same tree — real case: a job told
        // not to touch `*.md` reported `wrote:` full of two other jobs' markdown
        // edits). `git_changed` stays the full diff for `should_run_verify`.
        let git_changed = match (&git_before, agy::git_snapshot(&workspace)) {
            (Some(b), Some(a)) => Some(agy::git_changed_paths(b, &a)),
            _ => None,
        };
        let also_changed = git_changed
            .as_deref()
            .map(|c| git_only_paths(c, &r.artifacts, &workspace))
            .unwrap_or_default();
        // This run's own `run_command` lines, off the transcript on disk —
        // feeds the repeated-command warning and the stray-artifact
        // attribution check below. Empty (not an error) when the conv dir
        // can't be resolved, same posture as the other on-disk-only reads.
        let run_cmds = r
            .conv_id
            .as_deref()
            .and_then(agy::resolve_conv_dir)
            .map(|dir| agy::run_command_lines_for(&dir))
            .unwrap_or_default();
        // Stray left-behind files (`*.old`/`*.tmp`/…) attributable to THIS
        // run (docs/feedback-2026-09-15 P2) get pulled out of the generic
        // "also changed" bucket and named on their own — the rest of
        // `also_changed` stays exactly as before.
        let stray = stray_artifacts(&also_changed, &r.artifacts, &run_cmds);
        let also_changed: Vec<String> = also_changed
            .into_iter()
            .filter(|p| !stray.contains(p))
            .collect();
        let stray_short: Vec<String> = stray.iter().map(|p| short_path(p, &workspace)).collect();
        // Only worth a disk read when there's an unattributed diff to explain —
        // an empty `also_changed` never renders a footer segment that needs it.
        let overlapping_jobs = if also_changed.is_empty() {
            Vec::new()
        } else {
            let end = unix(SystemTime::now());
            let records = agy::list_job_records();
            agy::overlapping_jobs(&job_id, &ws_str, run_start_unix, end, &records)
        };
        // Punt nudge: a task whose preset promised file changes, reporting done
        // having left no write receipts, is suspicious — e.g. it only ran
        // `cargo add` and stopped short of the actual implementation. See
        // `punt_evidence` for which evidence counts and why an unattributed,
        // non-overlapping git diff does NOT (the run likely worked through shell
        // commands agy's own transcript doesn't receipt).
        if expects_changes {
            if let Some(evidence) = punt_evidence(
                !r.artifacts.is_empty(),
                git_changed.as_deref(),
                &overlapping_jobs,
            ) {
                let how = match evidence {
                    PuntEvidence::NoGit => "it created/edited no files".to_string(),
                    PuntEvidence::CleanGitTree => {
                        "the workspace's git tree is unchanged".to_string()
                    }
                    PuntEvidence::OverlappingJobsOnly => format!(
                        "the only workspace changes during this run overlap with other jobs \
                         ({}) and can't be attributed to it",
                        overlapping_jobs.join(", ")
                    ),
                };
                text = punt_nudge_text(&how, &text);
            }
        }
        // Supervisor-side check (preset `verify`). Skipped when git can prove the run
        // changed nothing: there is nothing new to check, and a read-only investigation
        // must not pay for a build. Outside a repo it always runs — an empty diff from a
        // non-repo is an absence of evidence, not evidence of absence (same reasoning as
        // the punt nudge above).
        let verify_line = match &verify {
            Some(cmd) if should_run_verify(git_changed.as_deref()) => {
                Some(agy::run_verify_command(cmd, &workspace, &preset_env).await)
            }
            _ => None,
        };
        // `wrote:` line-count/EOL notes (docs/feedback-2026-09-15 P1): only
        // meaningful in a git workspace, where there's a HEAD blob to diff
        // against.
        let (line_notes, eol_changed) = if git_changed.is_some() {
            gather_wrote_notes(&workspace, &r.artifacts)
        } else {
            (vec![None; r.artifacts.len()], Vec::new())
        };
        let outside = scope
            .as_deref()
            .map(|sc| {
                outside_scope(&r.artifacts, &workspace, sc, &agy::reports_dir())
                    .iter()
                    .map(|p| short_path(p, &workspace))
                    .collect()
            })
            .unwrap_or_default();
        let missing_expected = expect_files
            .as_deref()
            .map(|expected| {
                missing_expected_files(expected, &workspace, |p| p.exists())
                    .iter()
                    .map(|p| short_path(p, &workspace))
                    .collect()
            })
            .unwrap_or_default();
        let footer_notes = FooterNotes {
            line_notes,
            eol_changed,
            repeated_command: agy::repeated_command_warning(&run_cmds),
            stray_artifacts: stray_short,
            outside_scope: outside,
            missing_expected,
        };
        Ok(with_footer(
            text,
            &r.step_summary,
            &r.artifacts,
            &also_changed,
            &overlapping_jobs,
            git_changed.is_some(),
            r.ctx_tokens,
            &workspace,
            r.via,
            verify_line.as_deref(),
            &footer_notes,
        ))
    }

    /// Register a job and spawn its future DETACHED, returning (job_id, result rx).
    /// Detaching the run from the request future is the key property: a client-side
    /// request timeout (-32001) or cancellation can't drop/kill the run — it finishes
    /// and lands in the registry, recoverable via agy_poll. [`Self::dispatch`] is the
    /// only caller: it races the receiver against the inline window.
    /// `job_id` comes from [`agy::claim_job_id`] and is claimed by the caller, so
    /// the run future can carry it too (that's how the run reports its conversation
    /// id and liveness back into the record while it's still in flight).
    async fn spawn_job<F>(
        &self,
        job_id: String,
        spec: DispatchSpec,
        fut: F,
    ) -> (String, oneshot::Receiver<anyhow::Result<String>>)
    where
        F: Future<Output = anyhow::Result<String>> + Send + 'static,
    {
        let started = SystemTime::now();
        let tool = spec.tool;
        let prompt_head = spec.prompt_head.clone();
        {
            let mut st = self.state.lock().await;
            st.jobs.insert(
                job_id.clone(),
                Job {
                    status: JobStatus::Running,
                    tool,
                    model: spec.model.clone(),
                    started,
                    finished: None,
                    prompt_head: prompt_head.clone(),
                    result: None,
                    seen: false,
                },
            );
            st.last_job = Some(job_id.clone());
            // Bound the in-memory registry: drop finished jobs (preferring seen ones first).
            if st.jobs.len() > 50 {
                let over = st.jobs.len() - 50;
                for jid in jobs_to_evict(&st.jobs, over) {
                    st.jobs.remove(&jid);
                }
            }
        }
        // Mirror to disk so a separate `agy-mcp wait <id>` process can watch this
        // run to completion (the in-memory registry above is per-process). Written
        // BEFORE the run starts, and carrying the full dispatch spec: if this job
        // dies with its supervisor, `agy-mcp show <id>` still has everything needed
        // to resume or re-dispatch it, so nothing has to be re-derived from tokens.
        agy::write_job_record(&agy::JobRecord {
            id: job_id.clone(),
            status: "running".into(),
            tool: tool.into(),
            model: spec.model,
            started_unix: unix(started),
            finished_unix: None,
            prompt_head,
            result: None,
            prompt: spec.prompt,
            workspace: spec.workspace,
            add_dirs: spec.add_dirs,
            preset: spec.preset,
            conversation_id: spec.conversation_id,
            owner_pid: Some(std::process::id()),
            last_activity_unix: Some(unix(started)),
            seen: false,
            last_step: None,
        });
        let (tx, rx) = oneshot::channel();
        let server = self.clone();
        let jid = job_id.clone();
        tokio::spawn(async move {
            let res = fut.await;
            // A failed run is worth resuming far more often than re-running: the
            // sub-agent's context (and whatever it already did) is still in its
            // conversation. The record knows which one — attach that recovery
            // spec to the failure so every consumer (inline caller, agy_poll,
            // `agy-mcp wait`) gets it, not just the caller who happens to be alive.
            let disk = agy::read_job_record(&jid);
            let res = res.map_err(|e| {
                match disk
                    .as_ref()
                    .map(|r| r.recovery_hint_after(&format!("{e:#}")))
                {
                    Some(hint) => e.context(hint),
                    None => e,
                }
            });
            {
                let mut st = server.state.lock().await;
                if let Some(j) = st.jobs.get_mut(&jid) {
                    j.finished = Some(SystemTime::now());
                    match &res {
                        Ok(text) => {
                            j.status = JobStatus::Done;
                            j.result = Some(text.clone());
                        }
                        Err(e) => {
                            j.status = JobStatus::Failed;
                            j.result = Some(format!("{e:#}"));
                        }
                    }
                }
            }
            // Mirror the terminal state to disk for `agy-mcp wait` / a restarted
            // server's agy_poll. Preserves the started/tool/model fields.
            if let Some(mut rec) = disk {
                rec.finished_unix = Some(unix(SystemTime::now()));
                match &res {
                    Ok(text) => {
                        rec.status = "done".into();
                        rec.result = Some(text.clone());
                    }
                    Err(e) => {
                        rec.status = "failed".into();
                        rec.result = Some(format!("{e:#}"));
                    }
                }
                agy::write_job_record(&rec);
            }
            let quota_wall = matches!(&res, Err(e) if agy::is_quota_wall(&format!("{e:#}")));
            // The inline waiter may have gone away — fine.
            let _ = tx.send(res);
            // The run's permit is released by now, so this is the moment the server
            // can tell whether it's idle. No-ops unless the quota cache is stale (or this
            // run hit a quota wall) AND nothing else is running.
            server.maybe_refresh_quota(quota_wall);
        });
        (job_id, rx)
    }

    /// Resolve `agy_sub`'s workspace: explicit arg → the client's MCP roots →
    /// `AGY_WORKSPACE` → error.
    ///
    /// It deliberately never falls through to the scratch dir. Invariant #5 made
    /// `workspace` required precisely because a silent scratch default sends agy
    /// off burning quota hunting for a project that isn't there; asking the client
    /// where it is removes the typing without reintroducing the guess. Two or more
    /// roots is an error, not a coin flip — the caller knows which it meant.
    // Roots are deprecated (SEP-2577) and a 2026-07-28 client has none, but the
    // legacy-session clients (Claude Code, Claude Desktop) still answer them.
    #[allow(deprecated)]
    async fn resolve_workspace(&self, explicit: Option<String>) -> Result<PathBuf, ErrorData> {
        if let Some(w) = explicit.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            return Ok(PathBuf::from(w));
        }
        let (peer, offers_roots) = {
            let st = self.state.lock().await;
            (st.peer.clone(), st.client_roots.unwrap_or(false))
        };
        if let (Some(peer), true) = (peer, offers_roots) {
            if let Ok(res) = peer.list_roots().await {
                let dirs: Vec<PathBuf> = res
                    .roots
                    .iter()
                    .filter_map(|r| root_uri_to_path(&r.uri))
                    .collect();
                match dirs.len() {
                    1 => return Ok(dirs.into_iter().next().expect("len checked")),
                    n if n > 1 => {
                        let list = dirs
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(ErrorData::internal_error(
                            format!(
                                "workspace is ambiguous — the client offers {n} roots ({list}). \
                                 Pass workspace explicitly."
                            ),
                            None,
                        ));
                    }
                    _ => {}
                }
            }
        }
        if let Ok(w) = std::env::var("AGY_WORKSPACE") {
            let w = w.trim();
            if !w.is_empty() {
                return Ok(PathBuf::from(w));
            }
        }
        Err(ErrorData::internal_error(
            "workspace is required and could not be inferred: this client offers no MCP roots \
             and AGY_WORKSPACE is unset. Pass an absolute workspace path — a wrong one makes the \
             sub-agent burn quota hunting for the project."
                .to_string(),
            None,
        ))
    }

    /// The one dispatch shape: register + detach the run, wait a short inline window
    /// for it, and hand back a job ack if it's still going.
    ///
    /// There is deliberately no sync/async choice for the caller to make. A caller
    /// can't know in advance which side of the window a task will land on, so the
    /// choice was guesswork that cost a client-side `-32001` when it guessed wrong;
    /// now the request always returns well inside any client timeout, and a run that
    /// outlives the window is recoverable rather than lost.
    async fn dispatch<F>(
        &self,
        job_id: String,
        spec: DispatchSpec,
        fut: F,
    ) -> Result<String, ErrorData>
    where
        F: Future<Output = anyhow::Result<String>> + Send + 'static,
    {
        let tool = spec.tool;
        let (jid, rx) = self.spawn_job(job_id, spec, fut).await;
        // The run itself stays detached either way, so waiting here can't make a
        // cancelled request kill it — the only thing being chosen is whether this
        // call carries the result or a receipt.
        let client = self.state.lock().await.client_name.clone();
        let waited = match inline_wait_s(client.as_deref()) {
            Some(secs) => tokio::time::timeout(Duration::from_secs(secs), rx)
                .await
                .map_err(|_| ()),
            None => Ok(rx.await),
        };
        match waited {
            // Returned inline (ok or error) — collected either way, so it must not
            // also raise the uncollected-job banner later.
            Ok(Ok(res)) => {
                self.mark_seen(&jid).await;
                let text = res.map_err(to_err)?;
                Ok(self.with_pending_banner(text).await)
            }
            Ok(Err(_)) => Err(ErrorData::internal_error(
                "job result channel dropped".to_string(),
                None,
            )),
            Err(_) => Ok(self.with_pending_banner(self.job_ack(tool, &jid)).await),
        }
    }

    /// Ack handed back when a run outlives the inline window.
    fn job_ack(&self, tool: &'static str, job_id: &str) -> String {
        // Runs go in parallel up to AGY_MAX_CONCURRENT; if no permits are free right
        // now the job waits for a slot. (Advisory: permits can change before the job
        // grabs one.)
        // Prefer the CLI-wait over MCP polling: running `agy-mcp wait <id>` as a
        // background task blocks until done with ZERO LLM tokens, vs repeated
        // agy_poll round-trips that each cost context. agy_poll stays for a quick
        // one-shot check.
        let wait = format!(
            "NEXT ACTION REQUIRED: run in the background NOW: `agy-mcp wait {job_id}` (blocks \
             until done, zero tokens, then returns the report). Do this before moving on — \
             otherwise this job's result has to wait for you to come back and agy_poll it, and \
             it's easy to lose track of (e.g. across a session interruption). Once it runs, \
             don't read its output file or agy_poll — you are woken when it ends; with nothing \
             else to do, end your turn saying you're waiting on it. Without the wait, a one-shot check is agy_poll."
        );
        if self.concurrency.available_permits() == 0 {
            format!("started {tool} job {job_id} (at the concurrency limit — waits for a free slot). {wait}")
        } else {
            format!("started {tool} job {job_id} (running in parallel). {wait}")
        }
    }

    /// Compact warning for any job that finished (Done/Failed) but was never
    /// retrieved via agy_poll — `seen` stays false until that happens. Prepended
    /// to OTHER tool responses (agy_sub / agy_diag(status)) so a forgotten async
    /// job surfaces on the very next bridge call instead of staying invisible
    /// until someone happens to poll it. Bounded to the last 30 min so a job nobody
    /// is coming back for stops nagging.
    ///
    /// A candidate is cross-checked against the DISK record's `seen`, because the
    /// recommended collection path (`agy-mcp wait <id>`) is another process and can
    /// only clear the flag there. Without that check the banner warned about results
    /// the caller had already been handed (observed 2026-07-26) — and a banner that
    /// cries wolf gets tuned out, costing exactly the visibility it exists for.
    async fn pending_jobs_banner(&self) -> Option<String> {
        let st = self.state.lock().await;
        let now = SystemTime::now();
        let mut ids: Vec<String> = st
            .jobs
            .iter()
            .filter(|(_, j)| !j.seen && !matches!(j.status, JobStatus::Running))
            .filter_map(|(id, j)| {
                let age = now.duration_since(j.finished?).ok()?.as_secs();
                if age >= agy::WAIT_UNCOLLECTED_WINDOW_S {
                    return None;
                }
                if agy::read_job_record(id).is_some_and(|r| r.seen) {
                    return None;
                }
                Some(id.clone())
            })
            .collect();
        if ids.is_empty() {
            return None;
        }
        ids.sort();
        Some(format!(
            "⚠ uncollected job(s): {} — agy_poll",
            ids.join(",")
        ))
    }

    /// Mark a job's result as collected. Called when a run's terminal result is
    /// handed back **inline** (a sync run, or an async run that finished inside the
    /// fast-fail window) — the caller has seen it, so it must not show up in
    /// [`pending_jobs_banner`]. Without this every sync run leaves a `seen=false`
    /// finished job and the banner nags on the very next call about a result the
    /// caller already has.
    async fn mark_seen(&self, job_id: &str) {
        {
            let mut st = self.state.lock().await;
            if let Some(j) = st.jobs.get_mut(job_id) {
                j.seen = true;
            }
        }
        // Mirror it, so a sibling process (`agy-mcp wait`, or this server after a
        // restart) doesn't re-offer a result that has already been handed back.
        agy::mark_job_seen(job_id);
    }

    /// Prepend [`pending_jobs_banner`] to a successful tool response, if any.
    async fn with_pending_banner(&self, text: String) -> String {
        match self.pending_jobs_banner().await {
            Some(b) => format!("{b}\n\n{text}"),
            None => text,
        }
    }

    #[tool(
        description = "Delegate a task to an Antigravity sub-agent using your subscription quota. \
        Runs an autonomous agent with file access in the specified workspace."
    )]
    async fn agy_sub(
        &self,
        Parameters(a): Parameters<SubArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.note_client(&ctx).await;
        let a = SubArgs {
            model: normalize_opt_arg(a.model),
            model_label: normalize_opt_arg(a.model_label),
            conversation_id: normalize_opt_arg(a.conversation_id),
            workspace: normalize_opt_arg(a.workspace),
            mcp: normalize_opt_arg(a.mcp),
            preset: normalize_opt_arg(a.preset),
            ..a
        };
        let progress = progress_sink(&ctx);
        let ws = self.resolve_workspace(a.workspace).await?;
        if let Some(msg) = workspace_missing_message(&ws) {
            return Err(ErrorData::invalid_params(msg, None));
        }
        let mcp = parse_mcp(a.mcp);

        // `model_label` bypasses slot resolution, so a typo used to be discovered only
        // by agy, after the run was already paid for (the run log carries two:
        // `gemini-3.7-flash`, missing its effort suffix, n=2 ok=0%). It is resolved here
        // instead, before anything is dispatched — an exact slug as before, and now a
        // base or short name too (`resolve_by_name`; that dropped suffix now takes the
        // family's default effort). A miss re-lists once before it is believed.
        let pinned: Option<String> = match a
            .model_label
            .as_deref()
            .map(model_label_slug)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => Some(
                resolve_by_name_refreshing(raw)
                    .await
                    .map_err(|e| ErrorData::internal_error(e, None))?,
            ),
            None => None,
        };

        // Version resolution always takes the newest per slot (`resolve_sub` takes no
        // pin). Pinning an older one is not a `agy_sub` arg: the slug already names the
        // version exactly, so `model_label="gemini-3.5-pro-high"` is the pin, and it's
        // honest about being an exact-slug escape hatch instead of a second resolution
        // path.
        //
        // Resume vs fresh. A resume carries the conversation's model unless a model /
        // model_label is given, which SWITCHES the conversation to that model and
        // continues (cheaper than re-establishing context across agy model rotations).
        // run_core self-verifies the served model against `model` (the label we passed
        // agy), so no separate expected-label needs threading here.
        //
        // Everything that needs the network (slot resolution) or the disk (session
        // facts) happens here; the DECISION is `plan_dispatch`, which is pure and
        // unit-tested.
        let (switch, facts): (Option<String>, Option<ResumeFacts>) = match a
            .conversation_id
            .as_deref()
            .map(str::trim)
            .filter(|x| !x.is_empty())
        {
            Some(cid) => {
                let dir = agy::resolve_conv_dir(cid).ok_or_else(|| {
                    ErrorData::internal_error(format!("conversation not found: {cid}"), None)
                })?;
                // The switch target, if the caller asked to change model on resume.
                let switch: Option<String> = match pinned {
                    Some(l) => Some(l),
                    None => match a.model.as_deref() {
                        Some(m) => Some(
                            resolve_model_arg(m)
                                .await
                                .map_err(|e| ErrorData::internal_error(e, None))?,
                        ),
                        None => None,
                    },
                };
                let conv_id = dir
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .ok_or_else(|| {
                        ErrorData::internal_error("bad conversation dir".to_string(), None)
                    })?;
                let facts = ResumeFacts {
                    conv_id,
                    ctx_tokens: agy::session_ctx_tokens(&dir),
                    session_model: agy::session_model(&dir),
                    // Only consulted for a bare resume, and resolving it costs a
                    // cache read + a slot re-resolve — so skip it on a switch.
                    keep_model: match switch {
                        Some(_) => None,
                        None => resume_keep_model(&dir),
                    },
                };
                (switch, Some(facts))
            }
            None => {
                let label = match pinned {
                    Some(l) => l,
                    None => match a.model.as_deref() {
                        Some(m) => resolve_model_arg(m).await,
                        None => resolve_default_model().await,
                    }
                    .map_err(|e| ErrorData::internal_error(e, None))?,
                };
                (Some(label), None)
            }
        };
        let DispatchPlan {
            model,
            continue_id,
            model_disp,
            ctx_warn,
            ctx_limit,
        } = plan_dispatch(switch, facts, a.force)
            .map_err(|e| ErrorData::internal_error(e, None))?;

        // Cached-quota guard: refuse a dispatch we already strongly suspect will
        // 429 — zero extra latency (local file read, no agy launch). The reading it
        // consults now accrues on its own: a 429 receipt is recorded as a 0%
        // snapshot (`record_quota_wall`), and an idle server refreshes a stale cache
        // in the background (`maybe_refresh_quota`), so this no longer depends on
        // the caller having remembered to run `agy_diag(live)`. A reading stops
        // counting once a run on its group succeeds, or a scrape misses its row.
        // Both windows count: an exhausted week logs no 5h bucket at all.
        if !a.force {
            // agy itself broken beats any quota reading. Re-probe first (~2s, zero
            // quota): a repaired install must not need a server restart.
            if agy::agy_down().is_some() {
                agy::refresh_models_cache().await;
            }
            if let Some(why) = agy::agy_down() {
                return Err(ErrorData::internal_error(agy_down_text(&why), None));
            }
            if let Some(group) = model.as_deref().and_then(agy::quota_group_of) {
                if let Some(msg) = agy::cached_quota_unavailable(group) {
                    return Err(ErrorData::internal_error(msg, None));
                }
            }
        }

        // Build the run future once (owned, 'static) so it can be handed to
        // `dispatch` and outlive this request. The preset preamble is prepended here.
        //
        // Resolved against THIS workspace, so a project's own `.agy/presets.toml` wins.
        // A bad name or an unparseable file fails the call instead of degrading to no
        // preamble — the judgment written into a preset is the whole point of having it,
        // and losing it silently reads as the model getting worse.
        let preset = a.preset.unwrap_or_else(|| DEFAULT_PRESET.to_string());
        let table = presets::load_table(Some(&ws))
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))?;
        let resolved = presets::resolve_preset(&table, &preset)
            .map_err(|e| ErrorData::internal_error(format!("{e:#}"), None))?;
        let reports = agy::reports_dir().to_string_lossy().into_owned();
        // Cut BEFORE with_preamble consumes a.prompt: every preset preamble opens
        // with the same boilerplate, so a head cut after prepending it is the same
        // 60 chars for every job regardless of what the caller actually asked.
        let prompt_head: String = a.prompt.chars().take(60).collect();
        let prompt = with_preamble(&resolved.preamble, &ws, &reports, a.prompt);
        // {workspace}/{reports} substitution, same as the preamble — resolved
        // once here (before `reports` is moved into `add_dirs` below) so both the
        // agy child and its `verify` command see the same values (e.g. a lane's
        // CARGO_TARGET_DIR must match between the build and the check after it).
        let preset_env = presets::substituted_env(&resolved.env, &ws, &reports);
        let server = self.clone();
        let cap = default_max_answer();
        let mut add_dirs = a.add_dirs;
        if resolved.needs_reports_dir && !add_dirs.contains(&reports) {
            add_dirs.push(reports.clone());
        }
        let timeout_s = timeout_from_env(SUB_TIMEOUT_S);
        // Claim the job id up front so the RUN can write its conversation id and
        // liveness into the record while it's still in flight — that's what makes
        // a job that outlives its supervisor resumable instead of lost.
        let job_id = agy::claim_job_id();
        let spec = DispatchSpec {
            tool: "agy_sub",
            model: model_disp,
            prompt: prompt.clone(),
            prompt_head,
            workspace: ws.to_string_lossy().into_owned(),
            add_dirs: add_dirs.clone(),
            preset: preset.clone(),
            conversation_id: continue_id.clone(),
        };
        let run_jid = job_id.clone();
        let ws_for_note = ws.clone();
        let expects_changes = resolved.expects_changes;
        let verify = resolved.verify.clone();
        let agent = resolved.agent.clone();
        let scope = a.scope.or(resolved.scope).map(|list| {
            list.into_iter()
                .map(|s| {
                    s.replace("{reports}", &reports)
                        .replace("{workspace}", &ws.to_string_lossy())
                })
                .collect()
        });
        let expect_files = resolved.expect_files.map(|list| {
            list.into_iter()
                .map(|s| {
                    s.replace("{reports}", &reports)
                        .replace("{workspace}", &ws.to_string_lossy())
                })
                .collect()
        });
        // What the dispatching agent decided this run may reach. Carried to the
        // PreToolUse guard through agy's environment; None when the preset says
        // nothing, so the layered policy files stay the only source.
        let guard_policy = (!resolved.policy_deny.is_empty() || !resolved.policy_allow.is_empty())
            .then(|| {
                serde_json::json!({
                    "deny": resolved.policy_deny,
                    "allow": resolved.policy_allow,
                })
                .to_string()
            });
        let fut = async move {
            server
                .run_core(RunCoreOpts {
                    prompt,
                    model,
                    workspace: ws,
                    add_dirs,
                    continue_id,
                    mcp,
                    timeout_s,
                    max_answer_chars: cap,
                    tool: "agy_sub",
                    expects_changes,
                    strict_model: strict_model_preset(&preset),
                    verify,
                    progress,
                    ctx_warn,
                    ctx_limit,
                    guard_policy,
                    preset_env,
                    agent,
                    scope,
                    expect_files,
                    job_id: run_jid,
                })
                .await
        };

        // Taken before this job's own record exists, so it names only siblings.
        let shared = shared_tree_note_for(&job_id, &ws_for_note);
        let out = self.dispatch(job_id, spec, fut).await?;
        Ok(match shared {
            Some(note) => format!("{note}\n{out}"),
            None => out,
        })
    }

    #[tool(description = "Poll a background job started by agy_sub.")]
    async fn agy_poll(&self, Parameters(a): Parameters<PollArgs>) -> Result<String, ErrorData> {
        let a = PollArgs {
            job_id: normalize_opt_arg(a.job_id),
        };
        let mut st = self.state.lock().await;
        let explicit = a.job_id.as_deref().map(str::trim).filter(|x| !x.is_empty());
        let id = match explicit {
            Some(x) => x.to_string(),
            None => st.last_job.clone().ok_or_else(|| {
                ErrorData::internal_error("no job yet — start one with agy_sub".to_string(), None)
            })?,
        };
        let detail = match st.jobs.get_mut(&id) {
            Some(job) => {
                let status = match job.status {
                    JobStatus::Running => "running",
                    JobStatus::Done => "done",
                    JobStatus::Failed => "failed",
                };
                // Liveness and the current step both live only on the disk record
                // (the run writes them there), so read it from the mirror even on
                // the in-memory fast path — one read serves both fields.
                let disk = agy::read_job_record(&id);
                let silent = disk.as_ref().and_then(|r| r.silent_s());
                let last_step = disk.as_ref().and_then(|r| r.last_step.as_deref());
                let d = format_job_detail(
                    &id,
                    job.tool,
                    &job.model,
                    status,
                    job.secs(),
                    job.result.as_deref(),
                    &job.prompt_head,
                    silent,
                    last_step,
                    agy::job_is_waited(&id, &is_pid_alive),
                );
                // Terminal result actually handed back now — stop nagging about it,
                // in memory and on the mirror both (`mark_seen` can't be called
                // here: it takes the lock this scope already holds).
                if !matches!(job.status, JobStatus::Running) {
                    job.seen = true;
                    agy::mark_job_seen(&id);
                }
                d
            }
            // Not in memory (server restarted, losing the in-memory registry, or a
            // job only the disk mirror knows): fall back to the disk record.
            None => match agy::read_job_record(&id) {
                Some(rec) => {
                    let d = format_job_detail(
                        &id,
                        &rec.tool,
                        &rec.model,
                        &rec.status,
                        rec.secs(),
                        rec.result.as_deref(),
                        &rec.prompt_head,
                        rec.silent_s(),
                        rec.last_step.as_deref(),
                        agy::job_is_waited(&id, &is_pid_alive),
                    );
                    agy::mark_job_seen(&id);
                    d
                }
                None => {
                    return Err(ErrorData::internal_error(
                        format!("job not found: {id}"),
                        None,
                    ))
                }
            },
        };
        // Polling implicitly (no job_id) with several jobs around: return only the latest
        // job's result plus a one-line count of other jobs instead of the full roster.
        if explicit.is_none() && st.jobs.len() > 1 {
            let other_count = st.jobs.len() - 1;
            return Ok(format!(
                "Other background job(s): {other_count}\n\n{detail}"
            ));
        }
        Ok(detail)
    }

    #[tool(description = "Diagnostics (no quota): status, mcp, trace, sessions, live.")]
    async fn agy_diag(
        &self,
        Parameters(a): Parameters<DiagArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<String, ErrorData> {
        self.note_client(&ctx).await;
        let a = DiagArgs {
            conversation_id: normalize_opt_arg(a.conversation_id),
            ..a
        };
        match a.action {
            DiagAction::Status => {
                let mut s = agy::status().await;
                // Say whether `workspace` can be omitted on THIS client, so a
                // "workspace is required" error is self-explaining. Resolve it for
                // real rather than reading the advertised capability — see
                // `workspace_status_line`. One resolution serves both this line and
                // the preset list below, which is also why they can never disagree.
                let resolved = self.resolve_workspace(None).await;
                let who = self.state.lock().await.client_name.clone();
                s.push_str(&workspace_status_line(
                    who.as_deref(),
                    resolved.as_deref().map_err(|e| &*e.message),
                ));
                // A preset nobody can see is a preset nobody writes, so status names
                // both the available set and where to add to it. Resolved against the
                // same workspace agy_sub would use, since a project file only shows up
                // there. No workspace = built-ins + the global file, which is honest.
                let ws = resolved.ok();
                s.push_str(&format!(
                    "\npresets: {} (add: ~/.agy-mcp/presets.toml{})",
                    presets::list_names(ws.as_deref()),
                    ws.as_deref()
                        .map(|w| format!(", {}", presets::project_presets_path(w).display()))
                        .unwrap_or_default(),
                ));
                // Whether this machine's sub-agents run un-gated is invisible from the
                // outside and changes what a run is allowed to do, so it is stated.
                s.push_str(match agy::permission_mode() {
                    agy::PermissionMode::Gate => "\npermissions: gate (families pre-granted)",
                    agy::PermissionMode::Trusted => {
                        "\npermissions: TRUSTED — agy runs with --dangerously-skip-permissions (AGY_PERMISSIONS)"
                    }
                });
                let (skills, ws_mcp) = match ws.as_deref() {
                    Some(w) => {
                        let roots = agy::skill_roots(Some(w));
                        let sk = agy::check_skills(&roots);
                        let m = agy::workspace_mcp_servers(w);
                        (sk, Some(m))
                    }
                    None => {
                        let roots = agy::skill_roots(None);
                        let sk = agy::check_skills(&roots);
                        (sk, None)
                    }
                };
                s.push('\n');
                s.push_str(&agy::customizations_status(&skills, ws_mcp.as_ref()));
                Ok(self.with_pending_banner(s).await)
            }
            DiagAction::Mcp => {
                let mut s = agy::mcp_servers().await;
                if let Ok(ws) = self.resolve_workspace(None).await {
                    match agy::workspace_mcp_servers(&ws) {
                        Ok(servers) if !servers.is_empty() => {
                            s.push_str(&format!(
                                "\n\nworkspace MCP servers: {}",
                                servers.join(", ")
                            ));
                        }
                        Ok(_) => {
                            s.push_str("\n\nworkspace MCP servers: none");
                        }
                        Err(e) => {
                            s.push_str(&format!("\n\nworkspace MCP servers: {e}"));
                        }
                    }
                }
                Ok(s)
            }
            DiagAction::Trace => {
                let id = match a.conversation_id {
                    Some(x) if !x.trim().is_empty() => Some(x),
                    _ => self
                        .state
                        .lock()
                        .await
                        .last_conv_id
                        .clone()
                        .map(|arc| arc.to_string()),
                };
                let id = id.ok_or_else(|| {
                    ErrorData::internal_error(
                        "no run to trace — call agy_sub first, or pass conversation_id \
                         (see agy_diag action=sessions)"
                            .to_string(),
                        None,
                    )
                })?;
                let dir = agy::resolve_conv_dir(&id).ok_or_else(|| {
                    ErrorData::internal_error(format!("conversation not found: {id}"), None)
                })?;
                agy::read_trace(&dir, a.full).map_err(to_err)
            }
            DiagAction::Sessions => Ok(agy::list_sessions(a.limit.unwrap_or(12))),
            DiagAction::Live => {
                // Headless and quota-free (`/usage`, `agy models`): seconds, so the
                // call just awaits it under one ordinary permit.
                let _permit = self.acquire().await;
                Ok(match a.what {
                    LiveWhat::Quota => agy::live_quota().await,
                    LiveWhat::Models => agy::live_models().await.0,
                    LiveWhat::Both => agy::live_both().await,
                })
            }
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AgyServer {
    /// Capture the client and whether it offers `roots`. This is the only way to
    /// learn the user's project directory without guessing from the process cwd,
    /// which invariant #5 forbids (the server is desktop-launched, so its cwd is
    /// unrelated to whatever project the caller means).
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        {
            let mut st = self.state.lock().await;
            st.client_roots = Some(request.capabilities.roots.is_some());
            st.client_name = Some(format!(
                "{} {}",
                request.client_info.name, request.client_info.version
            ));
            st.peer = Some(context.peer.clone());
        }
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        let info = self.get_info();
        // A stale cache is what this session's instructions just showed; refresh it
        // for the refusals that follow. Detached and idle-only, so `initialize` never
        // waits on the 15–25s scrape.
        self.maybe_refresh_quota(false);
        Ok(info)
    }

    /// The instructions carry agy's cached state so the caller knows before its
    /// first call whether delegating is worth it. **Set once per session; never
    /// push a changed tool list or description mid-session** — they sit at the head
    /// of the caller's prompt cache, and `tools/list_changed` would invalidate all
    /// of it. Later news travels in the refusal text (`AGY_UNAVAILABLE:`/`AGY_DOWN:`).
    fn get_info(&self) -> ServerConfig {
        let mut instructions = String::from(
            "Bridge to Google's Antigravity CLI: run Gemini/Claude on your AI Pro/Ultra \
             subscription quota (zero metered billing). agy_sub = ask; agy_poll = collect a \
             run that outran the inline window; agy_diag = diagnostics (no quota).",
        );
        match agy::agy_down() {
            Some(why) => instructions += &format!("\nAGY_DOWN: {why}"),
            None => {
                if let Some(line) = agy::cached_status_line() {
                    instructions += "\n";
                    instructions += &line;
                }
            }
        }
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(instructions)
    }
}

/// Helper to check if a process with `pid` exists on Windows.
#[cfg(windows)]
pub(crate) fn is_pid_alive(pid: u32) -> bool {
    use std::os::windows::io::RawHandle;
    extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> RawHandle;
        fn CloseHandle(hObject: RawHandle) -> i32;
        fn WaitForSingleObject(hHandle: RawHandle, dwMilliseconds: u32) -> u32;
    }
    // SYNCHRONIZE (0x0010_0000) is what `WaitForSingleObject` needs, and asking for
    // it is not optional: with PROCESS_QUERY_LIMITED_INFORMATION (0x1000) alone the
    // wait fails with WAIT_FAILED and every process reads as dead — including this
    // one, which is how the unit test caught it. The pids checked here are sibling
    // agy-mcp servers under the same user, so the narrower openable set that the
    // extra right implies does not reach them.
    const SYNCHRONIZE_QUERY: u32 = 0x0010_0000 | 0x1000;
    unsafe {
        let handle = OpenProcess(SYNCHRONIZE_QUERY, 0, pid);
        if handle.is_null() || handle == (-(1isize) as RawHandle) {
            return false;
        }
        // Zero-timeout wait: WAIT_TIMEOUT (258) means the process object is not
        // signalled, i.e. still running. `GetExitCodeProcess` was the obvious way to
        // ask and is the classic trap — it reports STILL_ACTIVE (259) both for a live
        // process and for one that exited *with code 259*, so a job whose owner
        // returned 259 could never be demoted by `should_demote_job`. The handle's
        // signal state carries no such ambiguity.
        let res = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        res == 258
    }
}

#[cfg(not(windows))]
pub(crate) fn is_pid_alive(pid: u32) -> bool {
    // POSIX check using kill(pid, 0)
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Register this server instance (for `agy-mcp doctor`) — deliberately NOT an
/// exclusive lock.
///
/// The bridge used to refuse to start when another instance was alive, which
/// looked right (shared job registry, shared MCP config) but broke the way Claude
/// Code actually works: it spawns an MCP server **per session**, so the second
/// session onward silently got no `agy_*` tools at all, with no in-session way to
/// recover (observed 2026-07-26 — the workaround was killing the winner's process,
/// which can take its running jobs down with it).
///
/// Coexistence is safe now because the shared state is guarded where it actually
/// lives: job ids are claimed atomically on disk (`agy::claim_job_id`), records
/// carry an `owner_pid` so a starting server only reaps genuinely orphaned jobs,
/// and the MCP-config swap claims its breadcrumb with `create_new`. What stays
/// per-process is `AGY_MAX_CONCURRENT` — N sessions can run N × the limit; quota
/// is the real ceiling.
fn register_instance() {
    if let Err(e) = agy::register_server_pid(&is_pid_alive) {
        tracing::warn!("failed to register this server instance: {e}");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // CLI mode: `agy-mcp <subcommand> …` runs a one-shot command and exits. The
    // no-arg invocation (what Claude Code registers) falls through to the MCP
    // server below.
    let args: Vec<String> = std::env::args().collect();
    if let Some(sub) = args.get(1) {
        cli::run_cli(sub, &args[2..]); // never returns
    }

    // An agy that agy-mcp spawned started this server from agy's shared MCP config.
    // Nesting is not supported (D17): leave before the handshake, and agy runs on without us.
    if nest::inside_spawned_agy() {
        return Ok(());
    }

    // Announce this instance (diagnostics only — instances coexist by design).
    register_instance();

    // Logs go to stderr; stdout is reserved for the MCP stdio transport.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("agy-mcp starting (agy bin: {})", agy::agy_bin());
    // If a previous run crashed mid-MCP-swap, restore the original config now —
    // but never yank a swap that a live sibling server is still using.
    agy::recover_mcp_swap(&is_pid_alive);
    // Demote `running` job records whose OWNING process is gone, so `agy-mcp wait`
    // can't block forever on a job nobody is running. Owner-aware on purpose:
    // instances coexist, and a starting session must not fail a sibling's jobs.
    agy::mark_orphaned_running(&is_pid_alive);
    if let Err(e) = agy::ensure_verification_commands_allowed() {
        eprintln!("failed to ensure verification command grants: {e:#}");
    }
    // Keep the model cache current without anyone having to ask for it. Headless
    // (`agy models`), zero quota, detached so it never delays serving; a stale or
    // legacy-format cache is exactly what used to make the first agy_sub of a
    // session fail with "run agy_diag(live, what=models)". Run even when the cache
    // is fresh: the same call is the health probe behind `AGY_DOWN:`.
    tokio::spawn(async {
        let stale = agy::models_cache_needs_refresh();
        if agy::refresh_models_cache().await && stale {
            tracing::info!("model cache refreshed at startup");
        }
    });
    let service = AgyServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_substitutes_both_paths_and_keeps_the_task_separated() {
        let out = with_preamble(
            "Write findings under '{reports}'. The project is {workspace}.",
            Path::new("G:\\proj"),
            "C:\\rep",
            "do the thing".to_string(),
        );
        assert!(out.contains("under 'C:\\rep'"), "{out}");
        assert!(out.contains("project is G:\\proj"), "{out}");
        // The delimiter is what stops a preamble sentence from reading as part of the
        // task (and vice versa) — a preset is only worth having if it stays legible.
        assert!(out.ends_with("---\nTask:\ndo the thing"), "{out}");
        // An empty preamble must not add a delimiter to a bare prompt.
        assert_eq!(with_preamble("", Path::new("x"), "y", "ask".into()), "ask");
    }

    #[test]
    fn missing_workspace_is_refused_before_spawn() {
        let here = std::env::temp_dir();
        assert_eq!(workspace_missing_message(&here), None);
        let gone = here.join("agy-mcp-no-such-dir-8f1c");
        let msg = workspace_missing_message(&gone).unwrap();
        assert!(msg.starts_with("workspace does not exist: "), "{msg}");
    }

    #[test]
    fn review_preset_is_builtin_and_strict_about_the_model() {
        let t = presets::load_table(None).unwrap();
        let r = presets::resolve_preset(&t, "review").unwrap();
        assert!(r.preamble.contains("200 characters"));
        assert!(strict_model_preset("review") && !strict_model_preset("code"));
        let m = strict_model_message("gemini-4-argon-high", "Gemini 3.5 Flash", Some("abc"));
        assert!(
            m.contains("argon") && m.contains("Flash") && m.contains("abc"),
            "{m}"
        );
    }

    #[test]
    fn the_default_preset_name_exists_in_the_builtins() {
        // `DEFAULT_PRESET` is resolved like any other name, so a rename that missed the
        // built-in table would fail every bare agy_sub call at dispatch.
        let t = presets::parse_presets(include_str!("presets/builtin.toml")).unwrap();
        assert!(presets::resolve_preset(&t, DEFAULT_PRESET).is_ok());
    }

    fn slugs() -> Vec<String> {
        // A realistic `agy models` snapshot with two flash versions coexisting.
        [
            "gemini-3.6-flash-high",
            "gemini-3.6-flash-medium",
            "gemini-3.6-flash-low",
            "gemini-3.5-flash-high",
            "gemini-3.5-flash-low",
            "gemini-3.1-pro-high",
            "gemini-3.1-pro-low",
            "claude-sonnet-4-6",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// A resumed session, parameterized by the two facts the guards key on.
    fn facts(ctx: u64, session_model: &str, keep: Option<&str>) -> ResumeFacts {
        ResumeFacts {
            conv_id: "abc123".into(),
            ctx_tokens: ctx,
            session_model: Some(session_model.into()),
            keep_model: keep.map(str::to_string),
        }
    }

    #[test]
    fn fresh_run_dispatches_the_resolved_label() {
        let p = plan_dispatch(Some("gemini-3.6-flash-low".into()), None, false).unwrap();
        assert_eq!(p.model.as_deref(), Some("gemini-3.6-flash-low"));
        assert_eq!(p.continue_id, None); // fresh => agy starts a new conversation
        assert_eq!(p.model_disp, "gemini-3.6-flash-low");
        assert_eq!(p.ctx_warn, None);
        // A fresh run with nothing resolved is a bug, not a silent agy default.
        assert!(plan_dispatch(None, None, false).is_err());
    }

    #[test]
    fn bare_resume_keeps_the_sessions_own_model() {
        // No switch => pass the KEPT model, so agy can't default the thread to Flash.
        let p = plan_dispatch(
            None,
            Some(facts(
                30_000,
                "claude-sonnet-4-6",
                Some("claude-sonnet-4-6"),
            )),
            false,
        )
        .unwrap();
        assert_eq!(p.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(p.continue_id.as_deref(), Some("abc123"));
        // Named, not the bare "(resume)" — a supervisor watching several jobs
        // needs to tell a Sonnet resume apart from a Flash one.
        assert_eq!(p.model_disp, "(resume: claude-sonnet-4-6)");
        // A Flash session has no keep-model (agy's resume default is already
        // Flash), but the display still names it from the session's own record.
        let p = plan_dispatch(
            None,
            Some(facts(1_000, "gemini-3.6-flash-low", None)),
            false,
        )
        .unwrap();
        assert_eq!(p.model, None);
        assert_eq!(p.model_disp, "(resume: gemini-3.6-flash-low)");
    }

    #[test]
    fn bare_resume_falls_back_to_plain_resume_when_nothing_is_known() {
        // Neither the kept model nor the session's own recorded model is known
        // (e.g. an unreadable/legacy session) — only then does the display fall
        // back to the bare "(resume)".
        let f = ResumeFacts {
            conv_id: "abc123".into(),
            ctx_tokens: 1_000,
            session_model: None,
            keep_model: None,
        };
        let p = plan_dispatch(None, Some(f), false).unwrap();
        assert_eq!(p.model, None);
        assert_eq!(p.model_disp, "(resume)");
    }

    #[test]
    fn resume_with_a_model_switches_and_continues() {
        let p = plan_dispatch(
            Some("gemini-3.1-pro-high".into()),
            Some(facts(30_000, "gemini-3.6-flash-low", None)),
            false,
        )
        .unwrap();
        assert_eq!(p.model.as_deref(), Some("gemini-3.1-pro-high"));
        assert_eq!(p.continue_id.as_deref(), Some("abc123")); // same thread, new model
        assert_eq!(p.model_disp, "(resume→gemini-3.1-pro-high)");
    }

    #[test]
    fn a_heavy_flash_resume_is_reported_never_refused() {
        // This used to be a refusal. The dispatch now always goes through and the
        // caller is told; nothing about the session's size can cost a run.
        let heavy = || facts(FLASH_CONTINUE_LIMIT + 1, "gemini-3.6-flash-low", None);
        let p = plan_dispatch(None, Some(heavy()), false).unwrap();
        let w = p
            .ctx_warn
            .expect("a heavy Flash resume is worth a sentence");
        assert!(
            w.starts_with('\u{26a0}')
                && w.contains("fresh conversation")
                && !w.contains("model=opus")
                && !w.contains("model=pro"),
            "{w}"
        );
        assert_eq!(p.continue_id.as_deref(), Some("abc123"), "it still resumes");
        // force silences it, as it silences every guardrail.
        assert_eq!(
            plan_dispatch(None, Some(heavy()), true).unwrap().ctx_warn,
            None
        );
        // Switching OFF Flash is already the fix the notice recommends: stay quiet.
        assert_eq!(
            plan_dispatch(Some("gemini-3.1-pro-high".into()), Some(heavy()), false)
                .unwrap()
                .ctx_warn,
            None
        );
        // Switching Flash->Flash is not a fix, so the notice stands.
        assert!(
            plan_dispatch(Some("gemini-3.6-flash-high".into()), Some(heavy()), false)
                .unwrap()
                .ctx_warn
                .is_some()
        );
        // At the threshold exactly, and for a non-Flash session, it says nothing.
        assert_eq!(
            plan_dispatch(
                None,
                Some(facts(FLASH_CONTINUE_LIMIT, "gemini-3.6-flash-low", None)),
                false
            )
            .unwrap()
            .ctx_warn,
            None
        );
        assert_eq!(
            plan_dispatch(
                None,
                Some(facts(
                    FLASH_CONTINUE_LIMIT + 1,
                    "gemini-3.1-pro-high",
                    Some("gemini-3.1-pro-high")
                )),
                false
            )
            .unwrap()
            .ctx_warn,
            None
        );
    }

    #[test]
    fn the_after_the_run_notice_fires_only_above_the_threshold() {
        // The post-run half of the same threshold: reported, never enforced.
        let n = ctx_over_note(150_000, Some(FLASH_CONTINUE_LIMIT)).expect("over");
        assert!(
            n.contains("fresh conversation")
                && !n.contains("model=opus")
                && !n.contains("model=pro"),
            "{n}"
        );
        // At the threshold exactly, and with no threshold at all: silence.
        assert_eq!(
            ctx_over_note(FLASH_CONTINUE_LIMIT, Some(FLASH_CONTINUE_LIMIT)),
            None
        );
        assert_eq!(ctx_over_note(999_999, None), None);
        // Past Claude's own ceiling the notice still advises starting fresh.
        let n = ctx_over_note(224_500, Some(FLASH_CONTINUE_LIMIT)).expect("over");
        assert!(
            n.contains("fresh conversation")
                && !n.contains("model=opus")
                && !n.contains("model=pro"),
            "{n}"
        );
    }

    #[test]
    fn ctx_ceiling_warns_without_blocking() {
        // Non-Flash so the hard guard can't fire: over ~200k is advisory only.
        let p = plan_dispatch(
            None,
            Some(facts(
                200_001,
                "gemini-3.1-pro-high",
                Some("gemini-3.1-pro-high"),
            )),
            false,
        )
        .unwrap();
        assert!(p.ctx_warn.unwrap().contains("200k"));
        let p = plan_dispatch(
            None,
            Some(facts(
                200_000,
                "gemini-3.1-pro-high",
                Some("gemini-3.1-pro-high"),
            )),
            false,
        )
        .unwrap();
        assert_eq!(p.ctx_warn, None);
    }

    #[test]
    fn the_ctx_notice_threshold_is_flash_only_and_yields_to_force() {
        // Flash — whichever version or effort, and whether agy picks it for us.
        assert_eq!(
            ctx_limit_for(Some("gemini-3.6-flash-low"), false),
            Some(FLASH_CONTINUE_LIMIT)
        );
        assert_eq!(
            ctx_limit_for(Some("gemini-3.5-flash-high"), false),
            Some(FLASH_CONTINUE_LIMIT)
        );
        assert_eq!(ctx_limit_for(None, false), Some(FLASH_CONTINUE_LIMIT)); // agy defaults to Flash
                                                                            // Long-context models say nothing; so does anything we cannot classify.
        assert_eq!(ctx_limit_for(Some("gemini-3.1-pro-high"), false), None);
        assert_eq!(ctx_limit_for(Some("claude-sonnet-4-6"), false), None);
        assert_eq!(ctx_limit_for(Some("something-unknown"), false), None);
        // force opts out of the notice, on both halves of the threshold.
        assert_eq!(ctx_limit_for(Some("gemini-3.6-flash-low"), true), None);
        assert_eq!(ctx_limit_for(None, true), None);
    }

    #[test]
    fn dispatch_plan_carries_the_notice_threshold() {
        // Fresh Flash run: watched even though nothing is resumed.
        let p = plan_dispatch(Some("gemini-3.6-flash-low".into()), None, false).unwrap();
        assert_eq!(p.ctx_limit, Some(FLASH_CONTINUE_LIMIT));
        // Bare resume of a Flash session passes no --model, so agy picks Flash: watched.
        let p = plan_dispatch(
            None,
            Some(facts(1_000, "gemini-3.6-flash-low", None)),
            false,
        )
        .unwrap();
        assert_eq!(
            (p.model.as_deref(), p.ctx_limit),
            (None, Some(FLASH_CONTINUE_LIMIT))
        );
        // Switching to Pro on resume silences it for the rest of the thread.
        let p = plan_dispatch(
            Some("gemini-3.1-pro-high".into()),
            Some(facts(1_000, "gemini-3.6-flash-low", None)),
            false,
        )
        .unwrap();
        assert_eq!(p.ctx_limit, None);
    }

    #[test]
    fn gemini_prefers_newest_version_by_default() {
        let m = slugs();
        // No pin => newest (3.6), requested effort honored.
        assert_eq!(
            resolve_gemini_from(&m, agy::Slot::Flash, Effort::High, None).as_deref(),
            Some("gemini-3.6-flash-high")
        );
        assert_eq!(
            resolve_gemini_from(&m, agy::Slot::Flash, Effort::Low, None).as_deref(),
            Some("gemini-3.6-flash-low")
        );
    }

    #[test]
    fn gemini_version_pin_selects_that_version() {
        let m = slugs();
        assert_eq!(
            resolve_gemini_from(&m, agy::Slot::Flash, Effort::High, Some((3, 5))).as_deref(),
            Some("gemini-3.5-flash-high")
        );
        // Pinned 3.5 has no medium => effort ladder falls to the next available.
        assert_eq!(
            resolve_gemini_from(&m, agy::Slot::Flash, Effort::Medium, Some((3, 5))).as_deref(),
            Some("gemini-3.5-flash-high")
        );
    }

    #[test]
    fn gemini_effort_ladder_when_missing() {
        let m = slugs();
        // Pro (newest 3.1) has no medium; requesting medium falls to high.
        assert_eq!(
            resolve_gemini_from(&m, agy::Slot::Pro, Effort::Medium, None).as_deref(),
            Some("gemini-3.1-pro-high")
        );
    }

    /// `agy models` on 2026-10-16, every row.
    fn todays_catalog() -> Vec<String> {
        [
            "gemini-4-argon-medium",
            "gemini-4-argon-high",
            "gemini-4-argon-low",
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-medium",
            "gemini-3.8-flash-low",
            "claude-opus-5-5-low",
            "claude-opus-5-5-medium",
            "claude-opus-5-5-high",
            "claude-sonnet-5-5-low",
            "claude-sonnet-5-5-medium",
            "claude-sonnet-5-5-high",
            "gpt-oss-120b-medium",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn resolve_by_name_on_todays_catalog() {
        let m = todays_catalog();
        let r = |n: &str| resolve_by_name(n, &m);
        // A short name, the base name, a full slug, a display label.
        assert_eq!(r("argon").as_deref(), Ok("gemini-4-argon-high"));
        assert_eq!(r("gemini-4-argon").as_deref(), Ok("gemini-4-argon-high"));
        assert_eq!(r("gemini-4-argon-low").as_deref(), Ok("gemini-4-argon-low"));
        assert_eq!(
            r("Gemini 4 Argon (Medium)").as_deref(),
            Ok("gemini-4-argon-medium")
        );
        assert_eq!(r("argon-low").as_deref(), Ok("gemini-4-argon-low"));
        // The guide's one effort override (an alias + effort is a name, not the alias).
        assert_eq!(r("flash-low").as_deref(), Ok("gemini-3.8-flash-low"));
        assert_eq!(r("flash-low").as_deref(), Ok("gemini-3.8-flash-low"));
        // Slot floors: Claude and Flash medium, slot-less medium.
        assert_eq!(r("sonnet").as_deref(), Ok("claude-sonnet-5-5-medium"));
        assert_eq!(r("flash").as_deref(), Ok("gemini-3.8-flash-medium"));
        assert_eq!(r("gpt-oss").as_deref(), Ok("gpt-oss-120b-medium"));
        assert_eq!(
            r("gemini-3.8-flash").as_deref(),
            Ok("gemini-3.8-flash-medium")
        );
        // An effort the family doesn't have is refused, naming what it has.
        let e = r("gpt-oss-120b-low").unwrap_err();
        assert!(e.contains("gpt-oss-120b-medium"), "{e}");
        // More than one line: listed, not guessed.
        let e = r("claude").unwrap_err();
        assert!(
            e.contains("claude-opus-5-5") && e.contains("claude-sonnet-5-5"),
            "{e}"
        );
        let e = r("gemini").unwrap_err();
        assert!(e.contains("matches more than one"), "{e}");
        // Nothing: the catalog-miss message.
        let e = r("zzz").unwrap_err();
        assert!(e.contains("not in agy's model catalog"), "{e}");
        // An empty catalog passes the name through (the cold-cache rule).
        assert_eq!(resolve_by_name("argon", &[]).as_deref(), Ok("argon"));
    }

    #[test]
    fn resolve_by_name_takes_the_newest_version_and_refuses_two_lines() {
        let m: Vec<String> = [
            "gemini-3.7-flash-medium",
            "gemini-3.8-flash-medium",
            "claude-sonnet-4-6",
            "claude-sonnet-5-5-medium",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Two versions of one line: the newest.
        assert_eq!(
            resolve_by_name("flash", &m).as_deref(),
            Ok("gemini-3.8-flash-medium")
        );
        assert_eq!(
            resolve_by_name("sonnet", &m).as_deref(),
            Ok("claude-sonnet-5-5-medium")
        );
        // An older version is reachable by its base name.
        assert_eq!(
            resolve_by_name("gemini-3.7-flash", &m).as_deref(),
            Ok("gemini-3.7-flash-medium")
        );
        // A lite tier is a second line, so `flash` is ambiguous.
        let mut m2 = m.clone();
        m2.push("gemini-4-flash-lite-medium".into());
        let e = resolve_by_name("flash", &m2).unwrap_err();
        assert!(e.contains("gemini-4-flash-lite"), "{e}");
    }

    #[test]
    fn no_default_model_names_the_catalog_and_the_env() {
        let m = todays_catalog();
        let t = no_default_model_text("no 'flash' model in the cached list", &m);
        assert!(t.starts_with("no default model: no 'flash'"), "{t}");
        assert!(
            t.contains("gemini-4-argon, ") && t.contains("gpt-oss-120b"),
            "{t}"
        );
        assert!(!t.contains("-medium"), "{t}");
        assert!(t.contains("AGY_DEFAULT_MODEL=<name>"), "{t}");
        assert!(no_default_model_text("x", &[]).contains("catalog: (empty)"));
        assert!(default_model_line(None).starts_with("default model: flash"));
        assert!(default_model_line(Some("argon".into())).starts_with("default model: argon"));
    }

    #[test]
    fn bare_resume_keeps_a_slotless_model() {
        let m = todays_catalog();
        // A GPT-OSS conversation used to fall through to agy's default, Flash.
        assert_eq!(
            resume_keep_from("GPT-OSS 120B (Medium)", &m).as_deref(),
            Some("gpt-oss-120b-medium")
        );
        // Gone from the catalog → None (agy's default), as before.
        assert_eq!(resume_keep_from("Gemini 4 Xenon (High)", &m), None);
        assert_eq!(resume_keep_from("GPT-OSS 120B (Medium)", &[]), None);
        // The slot branches are unchanged.
        assert_eq!(resume_keep_from("Gemini 3.8 Flash (High)", &m), None);
        assert_eq!(
            resume_keep_from("Gemini 4 Argon (Medium)", &m).as_deref(),
            Some("gemini-4-argon-medium")
        );
        assert_eq!(
            resume_keep_from("Claude Opus 5.5 (Low)", &m).as_deref(),
            Some("claude-opus-5-5-low")
        );
        assert_eq!(
            resume_keep_from("Claude Sonnet 5.5 (High)", &m).as_deref(),
            Some("claude-sonnet-5-5-high")
        );
        // A pinned Pro generation that left the catalog resolves to nothing.
        assert_eq!(resume_keep_from("Gemini 3.1 Pro (High)", &m), None);
    }

    #[test]
    fn pro_is_the_argon_line_on_todays_list() {
        // `agy models` on 2026-10-16: 3.1 Pro is gone and Argon (a codename slug) is the
        // only Pro-slot model; `model=pro` defaults to high, and a pinned label resumes it.
        let m: Vec<String> = [
            "gemini-4-argon-medium",
            "gemini-4-argon-high",
            "gemini-4-argon-low",
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-medium",
            "claude-opus-5-5-medium",
            "gpt-oss-120b-medium",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            resolve_gemini_from(&m, agy::Slot::Pro, effective_effort(SubModel::Pro), None)
                .as_deref(),
            Some("gemini-4-argon-high")
        );
        assert_eq!(
            resolve_gemini_from(&m, agy::Slot::Pro, Effort::Low, None).as_deref(),
            Some("gemini-4-argon-low")
        );
        // The receipt label of a served argon run re-resolves to the same effort.
        assert_eq!(
            effort_from_label("Gemini 4 Argon (Medium)").slug(),
            "medium"
        );
        // And `model=flash` is untouched by it.
        assert_eq!(
            resolve_gemini_from(&m, agy::Slot::Flash, Effort::Medium, None).as_deref(),
            Some("gemini-3.8-flash-medium")
        );
    }

    #[test]
    fn gemini_bare_major_version_outranks_dotted() {
        // The next generation may drop the minor (`gemini-4-flash-*`). Reading that as
        // no version would leave `model=flash` on 3.8; a lite tier must not win either.
        let m: Vec<String> = [
            "gemini-3.8-flash-high",
            "gemini-3.8-flash-medium",
            "gemini-4-flash-lite-medium",
            "gemini-4-flash-high",
            "gemini-4-flash-medium",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            resolve_gemini_from(&m, agy::Slot::Flash, Effort::Medium, None).as_deref(),
            Some("gemini-4-flash-medium")
        );
        assert_eq!(
            resolve_gemini_from(&m, agy::Slot::Flash, Effort::Medium, Some((3, 8))).as_deref(),
            Some("gemini-3.8-flash-medium")
        );
    }

    #[test]
    fn claude_resolves_newest_version_at_the_requested_effort() {
        // agy 2026-10-03 lists low first; the old first-match took `-low`.
        let m: Vec<String> = [
            "gemini-3.8-flash-medium",
            "claude-opus-5-5-low",
            "claude-opus-5-5-medium",
            "claude-opus-5-5-high",
            "claude-sonnet-5-5-low",
            "claude-sonnet-5-5-medium",
            "claude-sonnet-5-5-high",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            resolve_claude_from(&m, ClaudeModel::Sonnet, Effort::High).as_deref(),
            Some("claude-sonnet-5-5-high")
        );
        assert_eq!(
            resolve_claude_from(&m, ClaudeModel::Opus, Effort::Low).as_deref(),
            Some("claude-opus-5-5-low")
        );
        // A coexisting older generation loses to the newer one.
        let mut mixed = m.clone();
        mixed.insert(0, "claude-sonnet-4-6".into());
        assert_eq!(
            resolve_claude_from(&mixed, ClaudeModel::Sonnet, Effort::High).as_deref(),
            Some("claude-sonnet-5-5-high")
        );
        // The pre-5.5 form (no effort suffix) still resolves.
        let old = vec![
            "claude-sonnet-4-6".to_string(),
            "claude-opus-4-6-thinking".to_string(),
        ];
        assert_eq!(
            resolve_claude_from(&old, ClaudeModel::Opus, Effort::High).as_deref(),
            Some("claude-opus-4-6-thinking")
        );
        assert_eq!(
            resolve_claude_from(
                &["gemini-3.8-flash-high".into()],
                ClaudeModel::Opus,
                Effort::High
            ),
            None
        );
    }

    #[test]
    fn gemini_unknown_pin_returns_none() {
        let m = slugs();
        assert_eq!(
            resolve_gemini_from(&m, agy::Slot::Flash, Effort::High, Some((9, 9))),
            None
        );
    }

    #[test]
    fn effort_and_variant_parse_from_labels() {
        assert_eq!(effort_from_label("gemini-3.5-flash-low").slug(), "low");
        assert_eq!(
            effort_from_label("Gemini 3.6 Flash (Medium)").slug(),
            "medium"
        );
        assert_eq!(effort_from_label("gemini-3.1-pro-high").slug(), "high");
        assert_eq!(effort_from_label("Gemini 3.6 Pro").slug(), "high"); // no suffix => high
        assert!(matches!(
            claude_variant_from_label("claude-opus-4-6-thinking"),
            ClaudeModel::Opus
        ));
        assert!(matches!(
            claude_variant_from_label("Claude Sonnet 4.6"),
            ClaudeModel::Sonnet
        ));
        // A resumed 5.5 conversation keeps its effort.
        assert_eq!(effort_from_label("Claude Sonnet 5.5 (Low)").slug(), "low");
        assert!(matches!(
            claude_variant_from_label("Claude Opus 5.5 (High)"),
            ClaudeModel::Opus
        ));
    }

    #[test]
    fn root_uri_to_path_handles_both_platforms() {
        // Windows: file:///C:/x — the slash before the drive letter must go.
        assert_eq!(
            root_uri_to_path("file:///G:/workshopG/gemini-subagent"),
            Some(PathBuf::from("G:/workshopG/gemini-subagent"))
        );
        // POSIX: the leading slash IS the path.
        assert_eq!(
            root_uri_to_path("file:///home/u/proj"),
            Some(PathBuf::from("/home/u/proj"))
        );
        // UNC-style host form, and percent-decoded spaces.
        assert_eq!(
            root_uri_to_path("file:///C:/My%20Code"),
            Some(PathBuf::from("C:/My Code"))
        );
        // Not a workspace: non-file schemes and empty paths yield nothing, so the
        // resolver falls through to AGY_WORKSPACE rather than inventing a dir.
        assert_eq!(root_uri_to_path("https://example.com/x"), None);
        assert_eq!(root_uri_to_path("file://"), None);
        assert_eq!(root_uri_to_path("/plain/path"), None);
    }

    #[test]
    fn liveness_sees_this_process_and_not_a_fabricated_one() {
        // `should_demote_job` fails a job only when its owner is gone, so a false
        // "alive" strands a dead job as running and a false "dead" fails a live
        // sibling's work. Pin both directions against the only two pids we can know.
        assert!(is_pid_alive(std::process::id()));
        // Windows pids are multiples of 4 and this is not one, so it can never be
        // assigned; on POSIX it is above the default pid_max.
        assert!(!is_pid_alive(u32::MAX - 1));
    }

    #[test]
    fn status_reports_the_resolution_not_the_advertised_capability() {
        // The failing case this exists for: a client that advertises `roots` and
        // then hands back nothing usable. The old line read the capability and
        // said "workspace optional" while the resolver had already failed.
        let line = workspace_status_line(
            Some("local-agent-mode-agy 1.0.0"),
            Err("workspace is required and could not be inferred: …"),
        );
        assert!(line.contains("REQUIRED"), "{line}");
        assert!(line.contains("could not be inferred"), "{line}");
        assert!(!line.contains("optional"), "{line}");

        // Resolved: name the actual path, so a run that lands in the wrong repo
        // is visible here rather than only in the sub-agent's confusion.
        let line = workspace_status_line(Some("cli 1.0"), Ok(Path::new("G:/workshopG/x")));
        assert!(line.contains("G:/workshopG/x"), "{line}");
        assert!(line.contains("arg optional"), "{line}");

        // Before `initialize` there is no client to describe either way.
        assert!(workspace_status_line(None, Err("whatever")).contains("not yet initialized"));
    }

    #[test]
    fn a_measured_client_acks_inside_its_request_bound() {
        // Claude Desktop reports `local-agent-mode-<server>` and cuts a request at
        // ~60s, so the ack has to land before that or every long run costs a
        // surprise error plus a manual collection.
        assert_eq!(client_inline_wait(Some("local-agent-mode-agy")), Some(45));
        assert_eq!(
            client_inline_wait(Some("local-agent-mode-anything")),
            Some(45)
        );
        // The Antigravity GUI cuts at 180s; it names itself `antigravity-client <ver>`.
        assert_eq!(
            client_inline_wait(Some("antigravity-client 1.0.0")),
            Some(150)
        );
        // Anything unmeasured waits for the result: returning the answer is better
        // than returning a job id, and assuming a bound nobody observed would give
        // up that outcome for free.
        assert_eq!(client_inline_wait(Some("claude-code")), None);
        assert_eq!(client_inline_wait(None), None);
    }

    #[test]
    fn a_pasted_model_listing_row_still_names_a_model() {
        // Verbatim from the run log (2026-08-14) — both halves of a `agy models`
        // row, tab-joined, requested as one label. It failed in ~6s.
        assert_eq!(
            model_label_slug("gemini-3.7-flash-low\tGemini 3.7 Flash (Low)"),
            "gemini-3.7-flash-low"
        );
        assert_eq!(
            model_label_slug("claude-sonnet-4-6\tClaude Sonnet 4.6 (Thinking)"),
            "claude-sonnet-4-6"
        );
        // A bare slug and a bare display label are both left alone: a display label
        // is legitimate input (wire_model passes one through), and it has spaces and
        // parentheses but never a tab.
        assert_eq!(
            model_label_slug("gemini-3.7-flash-low"),
            "gemini-3.7-flash-low"
        );
        assert_eq!(
            model_label_slug("  Gemini 3.7 Flash (Low)  "),
            "Gemini 3.7 Flash (Low)"
        );
        // The call sites filter on `is_empty`, so whitespace-only must land there
        // rather than becoming a `--model` argument of one space.
        assert_eq!(model_label_slug("   "), "");
    }

    #[test]
    fn effective_effort_per_model_default() {
        // Effort is not selectable per run — it's a fixed per-model floor. Flash
        // takes medium (2026-09-09; see the function's note on why it left low),
        // Claude medium (2026-10-03), Pro high. To override, name the slug:
        // model_label="gemini-3.8-flash-low".
        assert!(matches!(SubModel::from_alias("Pro"), Some(SubModel::Pro)));
        assert!(matches!(SubModel::from_alias("opus"), Some(SubModel::Opus)));
        assert!(SubModel::from_alias("argon").is_none());
        assert!(SubModel::from_alias("gemini-4-argon").is_none());
        assert_eq!(effective_effort(SubModel::Flash).slug(), "medium");
        assert_eq!(effective_effort(SubModel::Pro).slug(), "high");
        assert_eq!(effective_effort(SubModel::Sonnet).slug(), "medium");
        assert_eq!(effective_effort(SubModel::Opus).slug(), "medium");
    }

    #[test]
    fn test_jobs_to_evict_preference() {
        let mut jobs = HashMap::new();
        // Create 3 jobs: j1 (finished, seen), j2 (finished, unseen), j3 (running)
        jobs.insert(
            "j1".to_string(),
            Job {
                status: JobStatus::Done,
                tool: "agy_sub",
                model: "flash".to_string(),
                started: SystemTime::now(),
                finished: Some(SystemTime::now()),
                prompt_head: "test".to_string(),
                result: Some("ok".to_string()),
                seen: true,
            },
        );
        jobs.insert(
            "j2".to_string(),
            Job {
                status: JobStatus::Done,
                tool: "agy_sub",
                model: "flash".to_string(),
                started: SystemTime::now(),
                finished: Some(SystemTime::now()),
                prompt_head: "test".to_string(),
                result: Some("ok".to_string()),
                seen: false,
            },
        );
        jobs.insert(
            "j3".to_string(),
            Job {
                status: JobStatus::Running,
                tool: "agy_sub",
                model: "flash".to_string(),
                started: SystemTime::now(),
                finished: None,
                prompt_head: "test".to_string(),
                result: None,
                seen: false,
            },
        );

        // Evict 1 job: should pick j1 (finished, seen) over j2 (finished, unseen) or j3 (running)
        let evicted = jobs_to_evict(&jobs, 1);
        assert_eq!(evicted, vec!["j1".to_string()]);

        // Evict 2 jobs: should pick j1 first, then j2 (never j3 running)
        let evicted2 = jobs_to_evict(&jobs, 2);
        assert_eq!(evicted2, vec!["j1".to_string(), "j2".to_string()]);
    }

    /// The startup sweep must reap only jobs whose owner is really gone. Servers
    /// are per-session and coexist, so demoting a live sibling's running job would
    /// fail a run that is still working (and `agy-mcp wait` would report it dead).
    #[test]
    fn orphan_sweep_only_reaps_dead_owners() {
        let rec = |status: &str, owner: Option<u32>| agy::JobRecord {
            id: "j1".into(),
            status: status.into(),
            owner_pid: owner,
            ..Default::default()
        };
        let alive = |p: u32| p == 123;

        // Owner still running => leave it alone.
        assert!(!agy::should_demote_job(&rec("running", Some(123)), &alive));
        // Owner gone => orphaned, demote.
        assert!(agy::should_demote_job(&rec("running", Some(999)), &alive));
        // Legacy record with no owner => old behavior (demote).
        assert!(agy::should_demote_job(&rec("running", None), &alive));
        // Terminal records are never touched.
        assert!(!agy::should_demote_job(&rec("done", Some(999)), &alive));
        assert!(!agy::should_demote_job(&rec("failed", None), &alive));
    }

    #[test]
    fn test_timeout_from_env_str() {
        assert_eq!(timeout_from_env_str(None, SUB_TIMEOUT_S), 3600);
        assert_eq!(timeout_from_env_str(Some("600"), SUB_TIMEOUT_S), 600);
        assert_eq!(timeout_from_env_str(Some(""), SUB_TIMEOUT_S), 3600);
        assert_eq!(timeout_from_env_str(Some("abc"), SUB_TIMEOUT_S), 3600);
        assert_eq!(timeout_from_env_str(Some("0"), SUB_TIMEOUT_S), 3600);
        // the passed default is what an unset/bad value falls back to
        assert_eq!(timeout_from_env_str(None, 240), 240);
        assert_eq!(timeout_from_env_str(Some("90"), 240), 90);
    }

    #[test]
    fn verify_skips_only_when_git_proves_tree_unchanged() {
        // None: non-git workspace, absence of git evidence must not skip verify.
        assert!(should_run_verify(None));

        // Empty slice: git proves nothing changed, skip to avoid build costs on read-only runs.
        let empty: [String; 0] = [];
        assert!(!should_run_verify(Some(&empty)));

        // Non-empty slice: git saw modifications, verify must run.
        let changed = vec!["src/main.rs".to_string()];
        assert!(should_run_verify(Some(&changed)));
    }

    #[test]
    fn norm_path_for_match_folds_separators_and_windows_case() {
        assert_eq!(
            norm_path_for_match("src/main.rs"),
            norm_path_for_match(r"src\main.rs")
        );
        if cfg!(windows) {
            assert_eq!(
                norm_path_for_match(r"C:\Proj\Main.RS"),
                norm_path_for_match("c:/proj/main.rs")
            );
        }
    }

    /// A path git saw dirty that the transcript also claims is `wrote:`'s, not
    /// "also changed" — the two sources disagree only in separator/case, which
    /// must not make a real match look unattributed.
    #[test]
    fn git_only_paths_drops_paths_the_transcript_already_claimed() {
        let ws = Path::new(r"C:\proj");
        let artifacts = vec![ws
            .join("src")
            .join("main.rs")
            .to_string_lossy()
            .into_owned()];
        let git_changed = vec!["src/main.rs".to_string(), "docs/other.md".to_string()];
        let only = git_only_paths(&git_changed, &artifacts, ws);
        assert_eq!(
            only,
            vec![ws.join("docs/other.md").to_string_lossy().into_owned()]
        );
    }

    #[cfg(windows)]
    #[test]
    fn git_only_paths_matches_case_insensitively_on_windows() {
        let ws = Path::new(r"C:\proj");
        let artifacts = vec![r"C:\proj\src\MAIN.RS".to_string()];
        let git_changed = vec!["src/main.rs".to_string()];
        assert!(git_only_paths(&git_changed, &artifacts, ws).is_empty());
    }

    #[test]
    fn git_only_paths_empty_when_nothing_left_unattributed() {
        assert!(git_only_paths(&[], &[], Path::new(r"C:\proj")).is_empty());
    }

    /// The four branches `punt_evidence` can land on, and the one it must NOT:
    /// a receipt of any kind silences it outright, and an unattributed diff with
    /// no overlapping sibling is deliberately not proof of a punt (the run likely
    /// worked through shell commands agy's transcript doesn't receipt).
    #[test]
    fn punt_evidence_covers_every_branch() {
        // A receipt exists => never a punt, regardless of git.
        assert!(punt_evidence(true, None, &[]).is_none());
        assert!(punt_evidence(true, Some(&["a".to_string()]), &["j2".to_string()]).is_none());

        // No receipts, no git ground truth at all.
        assert!(matches!(
            punt_evidence(false, None, &[]),
            Some(PuntEvidence::NoGit)
        ));

        // No receipts, git proves an empty diff.
        let empty: [String; 0] = [];
        assert!(matches!(
            punt_evidence(false, Some(&empty), &[]),
            Some(PuntEvidence::CleanGitTree)
        ));

        // No receipts, a diff exists, but it all falls inside a sibling job's
        // window => can't be pinned on this run either.
        let changed = vec!["a.rs".to_string()];
        assert!(matches!(
            punt_evidence(false, Some(&changed), &["j2".to_string()]),
            Some(PuntEvidence::OverlappingJobsOnly)
        ));

        // No receipts, a diff exists, and nothing else was running here => this
        // run's own (shell-made) edit, not a punt.
        assert!(punt_evidence(false, Some(&changed), &[]).is_none());
    }

    /// Sample footer for the case the whole change exists to fix: a run with its
    /// own receipt, a sibling job's concurrent edit in the same tree, and the
    /// disk mirror naming who else was running.
    #[test]
    fn footer_splits_wrote_from_unattributed_concurrent_changes() {
        let ws = Path::new(r"C:\proj");
        let artifacts = vec![ws.join("src\\main.rs").to_string_lossy().into_owned()];
        let also_changed = vec![ws.join("ARCHITECTURE.md").to_string_lossy().into_owned()];
        let f = with_footer(
            "done".to_string(),
            "3 steps: ...",
            &artifacts,
            &also_changed,
            &["j46".to_string(), "j47".to_string()],
            true,
            1000,
            ws,
            "stdout",
            None,
            &FooterNotes::default(),
        );
        assert!(f.contains("wrote: src\\main.rs"), "{f}");
        assert!(
            f.contains("also changed during run (no write receipt): ARCHITECTURE.md"),
            "{f}"
        );
        assert!(
            f.contains("other jobs ran in this workspace meanwhile: j46, j47"),
            "{f}"
        );
        // Concurrent-edit paths are never folded into `wrote:` itself.
        assert!(!f.contains("wrote: src\\main.rs, ARCHITECTURE.md"), "{f}");
    }

    #[test]
    fn footer_marks_receipts_as_the_only_source_without_git() {
        let ws = Path::new(r"C:\proj");
        let artifacts = vec![ws.join("a.rs").to_string_lossy().into_owned()];
        let with_receipts = with_footer(
            "done".to_string(),
            "",
            &artifacts,
            &[],
            &[],
            false,
            0,
            ws,
            "",
            None,
            &FooterNotes::default(),
        );
        assert!(
            with_receipts.contains("wrote: a.rs (no git — receipts only)"),
            "{with_receipts}"
        );

        let no_receipts = with_footer(
            "done".to_string(),
            "1 steps: ...",
            &[],
            &[],
            &[],
            false,
            0,
            ws,
            "",
            None,
            &FooterNotes::default(),
        );
        assert!(
            no_receipts.contains("(no git — receipts only)"),
            "{no_receipts}"
        );
        assert!(!no_receipts.contains("wrote:"), "{no_receipts}");
    }

    /// Helper: assert that no entry under `properties` carries a `default` key.
    fn assert_no_property_defaults(schema: &schemars::Schema) {
        let props = schema
            .as_object()
            .and_then(|o| o.get("properties"))
            .and_then(|v| v.as_object());
        if let Some(props) = props {
            for (name, prop_schema) in props {
                if let Some(obj) = prop_schema.as_object() {
                    assert!(
                        !obj.contains_key("default"),
                        "property `{name}` still carries a `default` key: {obj:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn sub_args_schema_has_no_default_keys() {
        assert_no_property_defaults(&schemars::schema_for!(SubArgs));
    }

    #[test]
    fn poll_args_schema_has_no_default_keys() {
        assert_no_property_defaults(&schemars::schema_for!(PollArgs));
    }

    #[test]
    fn diag_args_schema_has_no_default_keys() {
        assert_no_property_defaults(&schemars::schema_for!(DiagArgs));
    }

    #[test]
    fn normalize_opt_arg_maps_placeholders_to_none() {
        assert_eq!(normalize_opt_arg(None), None);
        assert_eq!(normalize_opt_arg(Some("".to_string())), None);
        assert_eq!(normalize_opt_arg(Some("  ".to_string())), None);
        assert_eq!(normalize_opt_arg(Some("null".to_string())), None);
        assert_eq!(normalize_opt_arg(Some("NULL".to_string())), None);
        // "none" is a valid preset name — must not be swallowed.
        assert_eq!(
            normalize_opt_arg(Some("none".to_string())),
            Some("none".to_string())
        );
        assert_eq!(
            normalize_opt_arg(Some("abc".to_string())),
            Some("abc".to_string())
        );
        // Leading/trailing whitespace is trimmed but the value is kept.
        assert_eq!(
            normalize_opt_arg(Some("  abc  ".to_string())),
            Some("abc".to_string())
        );
    }

    #[test]
    fn fmt_paths_capped_with_notes_appends_a_note_only_where_one_exists() {
        let ws = Path::new(r"C:\proj");
        let paths = vec![
            ws.join("a.rs").to_string_lossy().into_owned(),
            ws.join("b.rs").to_string_lossy().into_owned(),
        ];
        let notes = vec![Some("new, 5 lines".to_string()), None];
        let s = fmt_paths_capped_with_notes(&paths, ws, &notes);
        assert_eq!(s, "a.rs (new, 5 lines), b.rs");
    }

    #[test]
    fn rel_under_workspace_strips_the_prefix_and_rejects_paths_outside_it() {
        let ws = Path::new(r"C:\proj");
        assert_eq!(
            rel_under_workspace(ws, r"C:\proj\src\main.rs"),
            Some("src/main.rs".to_string())
        );
        // Forward-slash input works the same as backslash.
        assert_eq!(
            rel_under_workspace(ws, "C:/proj/docs/x.md"),
            Some("docs/x.md".to_string())
        );
        // Outside the workspace (e.g. an add_dirs write): no HEAD blob to compare.
        assert_eq!(rel_under_workspace(ws, r"C:\other\a.rs"), None);
        // The workspace root itself has no "relative" remainder.
        assert_eq!(rel_under_workspace(ws, r"C:\proj"), None);
    }

    #[test]
    fn looks_stray_matches_the_documented_patterns_only() {
        assert!(looks_stray("docs/REFERENCE.md.old"));
        assert!(looks_stray("TTS-LINEUP.md.tmp"));
        assert!(looks_stray("backup~"));
        assert!(looks_stray("docs/REFERENCE_EDIT_REPORT.md"));
        assert!(looks_stray("notes_report.md"));
        assert!(looks_stray("thing.bak"));
        assert!(looks_stray("thing.orig"));
        assert!(looks_stray("tools/dev/tmp_probe.py"));
        assert!(looks_stray(r"tools\dev\__pycache__\patch.cpython-312.pyc"));
        assert!(looks_stray("tools/dev/__pycache__/"));
        assert!(!looks_stray("src/tmp.rs"));
        assert!(!looks_stray("docs/REFERENCE.md"));
        assert!(!looks_stray("src/main.rs"));
    }

    #[test]
    fn stray_artifacts_only_attributes_paths_this_run_can_explain() {
        let ws = Path::new(r"C:\proj");
        let old = ws
            .join("docs\\REFERENCE.md.old")
            .to_string_lossy()
            .into_owned();
        let tmp = ws.join("TTS-LINEUP.md.tmp").to_string_lossy().into_owned();
        let sibling_only = ws.join("other.old").to_string_lossy().into_owned();
        let git_only = vec![old.clone(), tmp.clone(), sibling_only.clone()];
        // `old` is named in a run_command this job issued; `tmp` has no such
        // mention and no receipt either — a sibling job's, not attributable.
        let run_commands = vec![format!("git show HEAD:docs/REFERENCE.md > {old}")];
        let out = stray_artifacts(&git_only, &[], &run_commands);
        assert_eq!(out, vec![old.clone()]);

        // A stray-shaped path that IS a write receipt is attributed even with
        // no matching run_command line.
        let out2 = stray_artifacts(&[], std::slice::from_ref(&tmp), &[]);
        assert_eq!(out2, vec![tmp]);

        // Bytecode is never named by the command that writes it: pinned on a run
        // that ran Python, left alone otherwise.
        let pyc = ws
            .join(r"tools\dev\__pycache__\patch.cpython-312.pyc")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            stray_artifacts(
                std::slice::from_ref(&pyc),
                &[],
                &["python -m unittest tools/dev/test_patch.py".to_string()]
            ),
            vec![pyc.clone()]
        );
        assert!(
            stray_artifacts(std::slice::from_ref(&pyc), &[], &["cargo test".to_string()])
                .is_empty()
        );

        // A normal source file never counts, however it got there.
        let normal = ws.join("src/main.rs").to_string_lossy().into_owned();
        assert!(stray_artifacts(
            std::slice::from_ref(&normal),
            std::slice::from_ref(&normal),
            &["cargo build".to_string()]
        )
        .is_empty());
    }

    #[test]
    fn footer_bits_renders_eol_repeated_command_and_stray_segments() {
        let ws = Path::new(r"C:\proj");
        let artifacts = vec![ws.join("docs\\DESIGN.md").to_string_lossy().into_owned()];
        let notes = FooterNotes {
            line_notes: vec![Some("new, 78 lines".to_string())],
            eol_changed: vec!["docs/API.md CRLF→LF".to_string()],
            repeated_command: Some("⚠ ran \"gen3dasset.exe sim\" 22 times".to_string()),
            stray_artifacts: vec!["docs/REFERENCE.md.old".to_string()],
            outside_scope: vec![],
            missing_expected: vec![],
        };
        let f = with_footer(
            "done".to_string(),
            "6 steps: ...",
            &artifacts,
            &[],
            &[],
            true,
            500,
            ws,
            "stdout",
            None,
            &notes,
        );
        assert!(f.contains("wrote: docs\\DESIGN.md (new, 78 lines)"), "{f}");
        assert!(f.contains("⚠ eol changed: docs/API.md CRLF→LF"), "{f}");
        assert!(f.contains("⚠ ran \"gen3dasset.exe sim\" 22 times"), "{f}");
        assert!(
            f.contains("⚠ stray artifacts left in workspace: docs/REFERENCE.md.old"),
            "{f}"
        );
    }

    #[test]
    fn punt_nudge_text_tells_a_read_only_task_the_warning_can_be_ignored() {
        let text = punt_nudge_text("the workspace's git tree is unchanged", "done");
        assert!(text.starts_with("⚠ this task reported done but"), "{text}");
        assert!(text.contains("may have punted"), "{text}");
        assert!(text.contains("preset=research"), "{text}");
        assert!(text.contains("read-only investigation"), "{text}");
        assert!(text.ends_with("\n\ndone"), "{text}");
    }

    #[test]
    fn a_watched_running_job_says_not_to_poll() {
        let line = |waited| {
            format_job_detail(
                "j1",
                "agy_sub",
                "flash",
                "running",
                30,
                None,
                "p",
                Some(5),
                None,
                waited,
            )
        };
        let watched = line(true);
        assert!(watched.contains("no need to poll"), "{watched}");
        assert!(!watched.contains("poll again later"), "{watched}");
        assert!(line(false).ends_with("(poll again later)"));
    }

    #[test]
    fn outside_scope_relative_and_absolute_and_component_boundary() {
        let ws = Path::new(r"C:\proj");
        let reports = Path::new(r"C:\reports");

        // relative scope
        let scope = vec!["src/core".to_string()];
        let artifacts = vec![
            r"C:\proj\src\core\a.rs".to_string(),
            r"C:\proj\src\other\b.rs".to_string(),
        ];
        assert_eq!(
            outside_scope(&artifacts, ws, &scope, reports),
            vec![r"C:\proj\src\other\b.rs".to_string()]
        );

        // relative artifact
        let rel_artifacts = vec!["src/core/a.rs".to_string(), "other/b.rs".to_string()];
        assert_eq!(
            outside_scope(&rel_artifacts, ws, &scope, reports),
            vec!["other/b.rs".to_string()]
        );

        // absolute scope
        let scope_abs = vec![r"C:\proj\src\core".to_string()];
        assert_eq!(
            outside_scope(&artifacts, ws, &scope_abs, reports),
            vec![r"C:\proj\src\other\b.rs".to_string()]
        );

        // component boundary (`src/core` does not contain `src/core2/b.rs`)
        let artifacts_boundary = vec![
            r"C:\proj\src\core\a.rs".to_string(),
            r"C:\proj\src\core2\b.rs".to_string(),
        ];
        assert_eq!(
            outside_scope(&artifacts_boundary, ws, &scope, reports),
            vec![r"C:\proj\src\core2\b.rs".to_string()]
        );

        // mixed `\` and `/`
        let scope_mixed = vec!["src/core".to_string()];
        let artifacts_mixed = vec![
            "C:/proj/src/core/a.rs".to_string(),
            r"C:\proj\src/core\sub\b.rs".to_string(),
            "C:/proj\\other/c.rs".to_string(),
        ];
        assert_eq!(
            outside_scope(&artifacts_mixed, ws, &scope_mixed, reports),
            vec!["C:/proj\\other/c.rs".to_string()]
        );

        // anything under reports is always in scope
        let artifacts_reports = vec![
            r"C:\reports\findings.md".to_string(),
            "C:/reports/sub/findings2.md".to_string(),
            r"C:\proj\other\c.rs".to_string(),
        ];
        assert_eq!(
            outside_scope(&artifacts_reports, ws, &scope, reports),
            vec![r"C:\proj\other\c.rs".to_string()]
        );

        // an empty scope puts everything out of scope (except under reports)
        let artifacts_empty_scope = vec![
            r"C:\proj\src\core\a.rs".to_string(),
            r"C:\proj\other\b.rs".to_string(),
        ];
        assert_eq!(
            outside_scope(&artifacts_empty_scope, ws, &[], reports),
            artifacts_empty_scope
        );
        let artifacts_empty_scope_with_reports = vec![
            r"C:\reports\findings.md".to_string(),
            r"C:\proj\src\core\a.rs".to_string(),
        ];
        assert_eq!(
            outside_scope(&artifacts_empty_scope_with_reports, ws, &[], reports),
            vec![r"C:\proj\src\core\a.rs".to_string()]
        );
    }

    #[test]
    fn outside_scope_resolves_dotdot_and_verbatim_prefix() {
        let ws = Path::new(r"C:\proj");
        let reports = Path::new(r"C:\reports");
        let scope = vec!["src".to_string()];
        let artifacts = vec![
            "src/../Cargo.toml".to_string(),
            "lib/../src/a.rs".to_string(),
            r"\\?\C:\proj\src\b.rs".to_string(),
        ];
        assert_eq!(
            outside_scope(&artifacts, ws, &scope, reports),
            vec!["src/../Cargo.toml".to_string()]
        );
    }

    #[cfg(windows)]
    #[test]
    fn outside_scope_ignores_case_on_windows() {
        let ws = Path::new(r"C:\proj");
        let reports = Path::new(r"C:\reports");
        let scope = vec!["SRC/CORE".to_string()];
        let artifacts = vec![
            r"c:\Proj\src\core\A.RS".to_string(),
            r"C:\PROJ\SRC\OTHER\B.RS".to_string(),
        ];
        assert_eq!(
            outside_scope(&artifacts, ws, &scope, reports),
            vec![r"C:\PROJ\SRC\OTHER\B.RS".to_string()]
        );
    }

    #[test]
    fn footer_bits_renders_outside_scope() {
        let ws = Path::new(r"C:\proj");
        let artifacts = vec![
            ws.join("src\\main.rs").to_string_lossy().into_owned(),
            ws.join("docs\\extra.md").to_string_lossy().into_owned(),
        ];
        let notes = FooterNotes {
            eol_changed: vec!["src/main.rs CRLF→LF".to_string()],
            outside_scope: vec!["docs/extra.md".to_string()],
            ..FooterNotes::default()
        };
        let f = with_footer(
            "done".to_string(),
            "2 steps: ...",
            &artifacts,
            &[],
            &[],
            true,
            100,
            ws,
            "stdout",
            None,
            &notes,
        );
        assert!(
            f.contains("⚠ eol changed: src/main.rs CRLF→LF · ⚠ outside scope: docs/extra.md"),
            "{f}"
        );
    }

    #[test]
    fn missing_expected_files_joins_relative_and_keeps_order() {
        let ws = Path::new(r"C:\proj");
        let expected = vec![
            "src/lib.rs".to_string(),
            r"C:\abs\file.txt".to_string(),
            "docs/readme.md".to_string(),
            "src/other.rs".to_string(),
        ];
        let exists = |p: &Path| {
            p == Path::new(r"C:\proj\src\lib.rs") || p == Path::new(r"C:\proj\src\other.rs")
        };
        let missing = missing_expected_files(&expected, ws, exists);
        assert_eq!(
            missing,
            vec![r"C:\abs\file.txt".to_string(), "docs/readme.md".to_string()]
        );
    }

    #[test]
    fn footer_bits_renders_missing_expected() {
        let ws = Path::new(r"C:\proj");
        let notes = FooterNotes {
            missing_expected: vec!["a.md".to_string(), "b.md".to_string()],
            ..FooterNotes::default()
        };
        let f = with_footer(
            "done".to_string(),
            "1 step",
            &[],
            &[],
            &[],
            true,
            100,
            ws,
            "stdout",
            Some("✓ verify: cargo check ok"),
            &notes,
        );
        assert!(
            f.contains("✗ expected file missing: a.md, b.md · ✓ verify: cargo check ok"),
            "{f}"
        );
    }
}
