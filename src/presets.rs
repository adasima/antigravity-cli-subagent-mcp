//! Presets — where the house's judgment lives, written once.
//!
//! Measured 2026-07-27 (`docs/ordering-conventions.md`): raising effort does not buy
//! correctness, it buys *judgment*, and a prompt with the judgment already written into
//! it beat the same task at high effort on both quality and cost. That result is only
//! usable if writing the judgment-free spec is cheap, and it isn't when the supervisor
//! retypes it per call — so the spec moves into a file the user edits once.
//!
//! Layering (later wins by name): built-in → `~/.agy-mcp/presets.toml` →
//! `<workspace>/.agy/presets.toml`. The built-ins are themselves written in the user's
//! dialect and parsed by this same code, so the format cannot drift from its
//! documentation and there is only one code path to get wrong.
//!
//! Everything except `load_table` is pure and unit-tested.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// The shipped defaults, in the same dialect a user writes.
const BUILTIN: &str = include_str!("presets/builtin.toml");

/// A preset as written in a file. The flags are `Option` so that `extends` can mean
/// "inherit" — a plain `false` has to stay distinguishable from "unspecified", or a
/// child could never keep a parent's `expects_changes` without restating it.
#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PresetDef {
    #[serde(default)]
    pub preamble: String,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub expects_changes: Option<bool>,
    #[serde(default)]
    pub needs_reports_dir: Option<bool>,
    /// Run this command in the workspace after a successful run and fold its
    /// result into the footer. Absent = off, which is every built-in.
    #[serde(default)]
    pub verify: Option<String>,
    /// Run a fresh run under this agy custom agent (`--agent <name>`), whose
    /// `tools:` list is enforced by agy. Absent = default agent.
    #[serde(default)]
    pub agent: Option<String>,
    /// Paths a run may write: prefixes, workspace-relative or absolute, with the
    /// `{workspace}`/`{reports}` placeholders. A write receipt outside them is
    /// flagged in the footer (never blocked or reverted, D13). First-wins across
    /// `extends`; an `agy_sub(scope=…)` argument replaces it. Absent = unchecked.
    #[serde(default)]
    pub scope: Option<Vec<String>>,
    /// Files a successful run must leave behind: paths, workspace-relative or
    /// absolute, with the `{workspace}`/`{reports}` placeholders. One that does not
    /// exist after the run is flagged in the footer (`✗ expected file missing: …`).
    /// First-wins across `extends`, like `scope`. Absent = unchecked.
    #[serde(default)]
    pub expect_files: Option<Vec<String>>,
    /// Extra command prefixes the PreToolUse guard should refuse for runs
    /// dispatched with this preset, and exceptions that beat the built-in list.
    ///
    /// This is how **the dispatching agent** defines the policy for the work it is
    /// handing over: the shape of a task is what decides which tools it has any
    /// business using, and that is a property of the preset, not of the machine.
    /// Unioned across the `extends` chain rather than first-wins — a child that
    /// tightens must not silently drop what its parent refused.
    #[serde(default)]
    pub policy_deny: Vec<String>,
    #[serde(default)]
    pub policy_allow: Vec<String>,
    /// Extra environment variables for the agy child process this preset dispatches
    /// AND for its own `verify` command — e.g. a per-lane `CARGO_TARGET_DIR` so
    /// parallel presets don't clobber one shared `target/` on Windows. Merged
    /// ancestor-first across `extends` (child wins per key), like the other flags.
    /// Values get the same `{workspace}`/`{reports}` substitution as `preamble`.
    /// Validated by [`validate_preset_env`] — see its doc for what's refused and why.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

pub(crate) type PresetTable = BTreeMap<String, PresetDef>;

/// A preset with inheritance applied — what the dispatch path actually uses.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedPreset {
    pub preamble: String,
    /// Arm the punt nudge: this preset promises the run will change files.
    pub expects_changes: bool,
    /// Push the findings dir onto this run's `add_dirs`.
    pub needs_reports_dir: bool,
    /// The supervisor-side check to run after the run, already vetted by
    /// [`verify_command_is_safe`].
    pub verify: Option<String>,
    /// Guard rules this preset adds for the run it dispatches (see
    /// [`PresetDef::policy_deny`]).
    pub policy_deny: Vec<String>,
    pub policy_allow: Vec<String>,
    /// Extra env for the agy child process and its `verify` command. NOT yet
    /// substituted (`{workspace}`/`{reports}`) — the caller applies
    /// [`substituted_env`] once it knows those paths, the same way `preamble`
    /// stays unsubstituted until `with_preamble` in main.rs.
    pub env: BTreeMap<String, String>,
    /// Custom agent for a fresh run (`--agent <name>`).
    pub agent: Option<String>,
    /// Paths a run may write (see [`PresetDef::scope`]). NOT yet substituted,
    /// like `env`.
    pub scope: Option<Vec<String>>,
    /// Files the run must leave behind (see [`PresetDef::expect_files`]). NOT yet
    /// substituted, like `scope`.
    pub expect_files: Option<Vec<String>>,
}

/// May the bridge itself run `cmd` after a run? Pure.
///
/// This is the same policy that decides what the SUB-AGENT may run
/// ([`crate::agy::is_safe_command_segment`]), and reusing it is the whole point: a
/// presets file lives in the workspace, so a repo you merely cloned could otherwise
/// hand the bridge a command the sub-agent running in that same repo would be
/// refused. Pipes, redirection and substitution are rejected with everything else —
/// `split_command_chain` returns None on them.
///
/// **Exactly one segment.** Chaining is allowed for the sub-agent because agy asks
/// for the whole line and we only decide yes/no. Here the bridge is the one running
/// it, and running a chain means either spawning a shell — the thing this policy
/// exists to avoid — or reimplementing `;`/`&&`/`||` semantics. Neither is worth it
/// for a check command, so a chain is refused with the reason.
pub(crate) fn verify_command_is_safe(cmd: &str) -> bool {
    match crate::agy::split_command_chain(cmd) {
        Some(segs) => segs.len() == 1 && crate::agy::is_safe_command_segment(segs[0]),
        None => false,
    }
}

/// An `extends` chain longer than this is a mistake, not a design.
const MAX_EXTENDS_DEPTH: usize = 8;

/// Env names a preset may never set, matched case-insensitively (Windows env names
/// are). A presets file arrives WITH the workspace, so a cloned repo must not be
/// able to hand itself an escape hatch through its own preset: `PATH`/`PATHEXT`/
/// `COMSPEC`/`PSMODULEPATH`/`SYSTEMROOT` are process-resolution primitives (widen
/// them and a "safe family" grant like `cargo` can be made to resolve to an
/// attacker-controlled binary); `RUSTC_WRAPPER`/`RUSTC_WORKSPACE_WRAPPER`/
/// `CARGO_BUILD_RUSTC_WRAPPER` hijack what `cargo build` actually executes; and
/// anything starting `AGY_` is this bridge's OWN control surface — it covers
/// `AGY_MCP_DELEGATED` and `AGY_MCP_POLICY` (the PreToolUse hook guard's inputs,
/// which `run_agy` sets from Rust, never from a file — see AGENTS.md's "The
/// sub-agent cannot forge either variable"), `AGY_PERMISSIONS` (the
/// `--dangerously-skip-permissions` opt-in), and the CLI's own `AGY_CLI_*`
/// (`AGY_CLI_DISABLE_AUTO_UPDATE`, the auto-update pin `run_agy` also sets).
const RESERVED_ENV_PREFIXES: &[&str] = &["AGY_"];
const RESERVED_ENV_NAMES: &[&str] = &[
    "PATH",
    "PATHEXT",
    "COMSPEC",
    "PSMODULEPATH",
    "SYSTEMROOT",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
];

/// Is `name` one of the reserved env vars a preset may not set? Pure, case-insensitive.
fn is_reserved_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    RESERVED_ENV_NAMES.contains(&upper.as_str())
        || RESERVED_ENV_PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// Is `name` a legal env-var identifier ([A-Za-z_][A-Za-z0-9_]*)? Pure.
fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Is `name` a legal agent name (1..=64 chars of ASCII alphanumerics, '-' or '_')? Pure.
fn is_valid_agent_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Validate one preset definition's `env` table. Pure — never touches disk.
///
/// Called for every layer as it loads (`load_table`), right after parsing and
/// before the whole-entry merge across files — that is the only point a bad key
/// can be pinned to the file that actually wrote it, since override-by-name means
/// the merged table no longer remembers which file a surviving entry came from.
/// It still runs well before any run is dispatched (`load_table` always precedes
/// `resolve_preset`, which always precedes spend), so the error is loud and precedes
/// the cost exactly like the `verify`-command check below.
///
/// Refuses: a key that isn't `[A-Za-z_][A-Za-z0-9_]*`, a reserved key (see
/// [`RESERVED_ENV_NAMES`]/[`RESERVED_ENV_PREFIXES`]), or a value containing a NUL
/// or newline (either breaks the child process's environment block outright).
pub(crate) fn validate_preset_env(preset: &str, file: &str, def: &PresetDef) -> Result<()> {
    for (key, value) in &def.env {
        if !is_valid_env_name(key) {
            bail!(
                "preset `{preset}` in {file}: env key `{key}` is not a valid name \
                 (must match [A-Za-z_][A-Za-z0-9_]*)"
            );
        }
        if is_reserved_env_name(key) {
            bail!(
                "preset `{preset}` in {file}: env key `{key}` is reserved and cannot be \
                 set by a preset (process-resolution, compiler-wrapper, or AGY_* control vars)"
            );
        }
        if value.contains('\0') || value.contains('\n') || value.contains('\r') {
            bail!(
                "preset `{preset}` in {file}: env value for `{key}` contains a NUL or \
                 newline, which cannot be passed to a child process environment"
            );
        }
    }
    Ok(())
}

/// Substitute the same path placeholders `preamble` gets (`{reports}` then
/// `{workspace}`) into every env value. Pure; mirrors `main.rs`'s `with_preamble`,
/// minus the `Task:` wrapper an env value has no use for.
pub(crate) fn substituted_env(
    env: &BTreeMap<String, String>,
    workspace: &Path,
    reports: &str,
) -> BTreeMap<String, String> {
    env.iter()
        .map(|(k, v)| {
            let v = v
                .replace("{reports}", reports)
                .replace("{workspace}", &workspace.to_string_lossy());
            (k.clone(), v)
        })
        .collect()
}

/// Parse one presets file. Pure.
pub(crate) fn parse_presets(raw: &str) -> Result<PresetTable> {
    toml::from_str(raw).context("not valid presets TOML")
}

/// Layer tables so a later one overrides an earlier one **by name**. Pure.
///
/// Override is whole-entry, not field-merge: redefining `code` replaces it outright.
/// Field-merging would make a project file that sets one flag silently inherit a
/// preamble it never saw; `extends = "code"` is the explicit way to say "and also".
pub(crate) fn merge_presets(layers: &[PresetTable]) -> PresetTable {
    let mut out = PresetTable::new();
    for layer in layers {
        for (k, v) in layer {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Resolve `name` against `table`, following `extends`. Pure.
///
/// An unknown name is an error listing what IS known — never a silent fallback to
/// `none`. A typo'd preset that quietly becomes "no preamble" is precisely the
/// low-quality mode this whole mechanism exists to avoid, and it would look like the
/// model got worse rather than like a typo.
pub(crate) fn resolve_preset(table: &PresetTable, name: &str) -> Result<ResolvedPreset> {
    // Walk child -> ancestor, collecting the chain.
    let mut chain: Vec<&PresetDef> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    let mut cur = name;
    loop {
        if seen.contains(&cur) {
            bail!("preset `{name}`: `extends` cycle through `{cur}`");
        }
        seen.push(cur);
        let def = table.get(cur).ok_or_else(|| {
            let known = table.keys().cloned().collect::<Vec<_>>().join(", ");
            let via = if cur == name {
                String::new()
            } else {
                format!(" (via `{name}`)")
            };
            anyhow::anyhow!("unknown preset `{cur}`{via}; available: {known}")
        })?;
        chain.push(def);
        if chain.len() > MAX_EXTENDS_DEPTH {
            bail!("preset `{name}`: `extends` chain deeper than {MAX_EXTENDS_DEPTH}");
        }
        match def.extends.as_deref() {
            Some(parent) => cur = parent,
            None => break,
        }
    }

    // Ancestor-first for text (the general rules read before the specific ones), but
    // child-first for flags (the nearest explicit answer wins).
    let preamble = chain
        .iter()
        .rev()
        .map(|d| d.preamble.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    // Refuse at resolve time, not at run time: the run has not been dispatched yet,
    // so the user finds out from a message naming the command instead of from a
    // footer line missing after a run they already paid for.
    let verify = chain.iter().find_map(|d| d.verify.clone());
    if let Some(ref cmd) = verify {
        if !verify_command_is_safe(cmd) {
            bail!(
                "preset `{name}`: verify command `{cmd}` is not one the bridge may run. \
                 It must be a SINGLE build/test-family command (no chaining, no pipes, \
                 no redirection, no substitution) — the same policy that governs what \
                 the sub-agent itself may run, minus the chaining the bridge would need \
                 a shell for."
            );
        }
    }
    let agent = chain.iter().find_map(|d| d.agent.clone());
    if let Some(ref a) = agent {
        if !is_valid_agent_name(a) {
            bail!(
                "preset `{name}`: agent `{a}` is not a valid agent name \
                 (must be 1..=64 chars of ASCII alphanumerics, '-' or '_')"
            );
        }
    }
    let scope = chain.iter().find_map(|d| d.scope.clone());
    let expect_files = chain.iter().find_map(|d| d.expect_files.clone());
    // Ancestor-first insert so the child's value for a shared key overwrites the
    // parent's, same rule as the flags above ("the nearest explicit answer wins").
    let mut env = BTreeMap::new();
    for d in chain.iter().rev() {
        for (k, v) in &d.env {
            env.insert(k.clone(), v.clone());
        }
    }
    Ok(ResolvedPreset {
        preamble,
        expects_changes: chain
            .iter()
            .find_map(|d| d.expects_changes)
            .unwrap_or(false),
        needs_reports_dir: chain
            .iter()
            .find_map(|d| d.needs_reports_dir)
            .unwrap_or(false),
        verify,
        // Unioned, not first-wins: a guard rule the parent set is a refusal, and a
        // child that never mentions it must not be the reason it stops applying.
        policy_deny: chain.iter().flat_map(|d| d.policy_deny.clone()).collect(),
        policy_allow: chain.iter().flat_map(|d| d.policy_allow.clone()).collect(),
        env,
        agent,
        scope,
        expect_files,
    })
}

/// Where a user-editable presets file lives for `workspace`.
pub(crate) fn project_presets_path(workspace: &Path) -> std::path::PathBuf {
    workspace.join(".agy").join("presets.toml")
}

/// Built-in + global + project layers. The only impure part of this module.
///
/// A file that exists but does not parse is a hard error naming it — loud, because it
/// is a file the user hand-edits and only they can fix, and because the alternative
/// (fall back to the built-in) silently discards the judgment they wrote and shows up
/// as "the model got worse". A file that is simply absent is not an error.
pub(crate) fn load_table(workspace: Option<&Path>) -> Result<PresetTable> {
    let builtin_table = parse_presets(BUILTIN).expect("built-in presets must parse");
    for (name, def) in &builtin_table {
        validate_preset_env(name, "built-in", def).expect("built-in presets must have valid env");
    }
    let mut layers = vec![builtin_table];
    let global = crate::agy::agy_mcp_home().join("presets.toml");
    let project = workspace.map(project_presets_path);
    for path in [Some(global), project].into_iter().flatten() {
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).context(format!("reading {}", path.display())),
        };
        let layer = parse_presets(&raw).with_context(|| format!("in {}", path.display()))?;
        let file_label = path.display().to_string();
        for (name, def) in &layer {
            validate_preset_env(name, &file_label, def)?;
        }
        layers.push(layer);
    }
    Ok(merge_presets(&layers))
}

/// Preset names available for `workspace`, for diagnostics. Never fails: a broken file
/// is reported as such rather than hiding the built-ins that still work.
pub(crate) fn list_names(workspace: Option<&Path>) -> String {
    match load_table(workspace) {
        Ok(t) => t.keys().cloned().collect::<Vec<_>>().join(", "),
        Err(e) => format!("<error: {e:#}>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(raw: &str) -> PresetTable {
        parse_presets(raw).expect("test fixture must parse")
    }

    #[test]
    fn builtins_parse_and_keep_their_promises() {
        let t = parse_presets(BUILTIN).expect("built-in presets must parse");
        for name in ["none", "code", "research", "report"] {
            assert!(t.contains_key(name), "built-in `{name}` went missing");
        }
        let code = resolve_preset(&t, "code").unwrap();
        assert!(code.expects_changes, "the punt nudge is armed by this flag");
        assert!(!code.needs_reports_dir);
        // The line continuations must actually join, or the preamble ships as ragged
        // half-sentences.
        assert!(code
            .preamble
            .contains("smallest focused change that satisfies"));
        for name in ["research", "report"] {
            let p = resolve_preset(&t, name).unwrap();
            assert!(
                p.needs_reports_dir,
                "`{name}` writes findings outside the repo"
            );
            assert!(
                p.preamble.contains("{reports}"),
                "substitution point survived parsing"
            );
        }
    }

    #[test]
    fn a_later_layer_replaces_an_earlier_one_by_name() {
        let base = table("[code]\npreamble = \"base\"\nexpects_changes = true\n");
        let over = table("[code]\npreamble = \"mine\"\n");
        let merged = merge_presets(&[base, over]);
        let r = resolve_preset(&merged, "code").unwrap();
        assert_eq!(r.preamble, "mine");
        // Whole-entry override, NOT a field merge: the flag is gone because the user's
        // definition never mentioned it.
        assert!(!r.expects_changes);
    }

    #[test]
    fn extends_puts_the_parent_first_and_lets_the_child_win_on_flags() {
        let t = table(
            "[code]\npreamble = \"parent rules\"\nexpects_changes = true\n\
             needs_reports_dir = true\n\n\
             [code-rs]\nextends = \"code\"\npreamble = \"child rules\"\n\
             needs_reports_dir = false\n",
        );
        let r = resolve_preset(&t, "code-rs").unwrap();
        assert_eq!(r.preamble, "parent rules\n\nchild rules");
        assert!(r.expects_changes, "unspecified in the child = inherited");
        assert!(
            !r.needs_reports_dir,
            "an explicit false in the child overrides"
        );
    }

    #[test]
    fn unknown_and_cyclic_names_fail_loudly_instead_of_defaulting() {
        let t = table("[none]\npreamble = \"x\"\n[a]\nextends = \"b\"\n[b]\nextends = \"a\"\n");
        let e = resolve_preset(&t, "cdoe").unwrap_err().to_string();
        assert!(e.contains("unknown preset `cdoe`"), "{e}");
        assert!(
            e.contains("none"),
            "the error must list what IS available: {e}"
        );
        let e = resolve_preset(&t, "a").unwrap_err().to_string();
        assert!(e.contains("cycle"), "{e}");
    }

    #[test]
    fn this_repos_own_house_preset_still_resolves() {
        // Dogfood check. Without it a typo in .agy/presets.toml would only surface when
        // someone dispatched a run against this repo — i.e. at the worst moment.
        let merged = merge_presets(&[
            parse_presets(BUILTIN).unwrap(),
            table(include_str!("../.agy/presets.toml")),
        ]);
        let r = resolve_preset(&merged, "house").unwrap();
        assert!(r.expects_changes, "inherited from `code`");
        // Resolving it at all proves the verify command clears the policy — a typo
        // there would otherwise only surface on a delegated run against this repo.
        assert_eq!(r.verify.as_deref(), Some("cargo check --all-targets"));
        assert!(
            r.preamble.contains("smallest focused change"),
            "parent text comes first"
        );
        assert!(
            r.preamble.contains("pure function"),
            "and the child's own rules follow"
        );
    }

    #[test]
    fn verify_is_inherited_and_overridden_like_the_flags() {
        let t = table(
            "[code]\npreamble = \"p\"\nverify = \"cargo check\"\n\n\
             [child]\nextends = \"code\"\npreamble = \"c\"\n\n\
             [grandchild]\nextends = \"child\"\npreamble = \"g\"\nverify = \"cargo test\"\n",
        );
        assert_eq!(
            resolve_preset(&t, "child").unwrap().verify.as_deref(),
            Some("cargo check"),
            "unspecified in the child = inherited"
        );
        assert_eq!(
            resolve_preset(&t, "grandchild").unwrap().verify.as_deref(),
            Some("cargo test"),
            "the nearest explicit answer wins"
        );
        assert_eq!(
            resolve_preset(&table("[a]\npreamble = \"x\"\n"), "a")
                .unwrap()
                .verify,
            None,
            "absent = off"
        );
    }

    #[test]
    fn agent_is_inherited_overridden_and_validated() {
        let builtin = parse_presets(BUILTIN).unwrap();
        assert_eq!(
            resolve_preset(&builtin, "research")
                .unwrap()
                .agent
                .as_deref(),
            Some("agy-mcp-research")
        );
        assert_eq!(resolve_preset(&builtin, "code").unwrap().agent, None);

        let t = table(
            "[parent]\npreamble = \"p\"\nagent = \"parent-agent\"\n\n\
             [child]\nextends = \"parent\"\npreamble = \"c\"\n\n\
             [override_child]\nextends = \"parent\"\npreamble = \"o\"\nagent = \"child-agent\"\n",
        );
        assert_eq!(
            resolve_preset(&t, "child").unwrap().agent.as_deref(),
            Some("parent-agent"),
            "a child inherits its parent's agent"
        );
        assert_eq!(
            resolve_preset(&t, "override_child")
                .unwrap()
                .agent
                .as_deref(),
            Some("child-agent"),
            "a child overrides it"
        );

        for bad in ["a b", "../x", "", "a".repeat(65).as_str(), "agent!"] {
            let t = table(&format!("[p]\npreamble = \"x\"\nagent = '{bad}'\n"));
            let e = resolve_preset(&t, "p").unwrap_err().to_string();
            assert!(
                e.contains("preset `p`") && e.contains(bad),
                "bad agent `{bad}` must be rejected naming preset and bad value: {e}"
            );
        }
    }

    #[test]
    fn scope_is_inherited_and_overridden() {
        let builtin = parse_presets(BUILTIN).unwrap();
        assert_eq!(
            resolve_preset(&builtin, "research").unwrap().scope,
            Some(vec!["{reports}".to_string()])
        );
        assert_eq!(resolve_preset(&builtin, "code").unwrap().scope, None);

        let t = table(
            "[parent]\npreamble = \"p\"\nscope = [\"src/core\"]\n\n\
             [child]\nextends = \"parent\"\npreamble = \"c\"\n\n\
             [override_child]\nextends = \"parent\"\npreamble = \"o\"\nscope = [\"src/other\"]\n",
        );
        assert_eq!(
            resolve_preset(&t, "child").unwrap().scope,
            Some(vec!["src/core".to_string()]),
            "a child inherits its parent's scope"
        );
        assert_eq!(
            resolve_preset(&t, "override_child").unwrap().scope,
            Some(vec!["src/other".to_string()]),
            "a child overrides it"
        );
    }

    #[test]
    fn expect_files_is_inherited_and_overridden() {
        let builtin = parse_presets(BUILTIN).unwrap();
        assert_eq!(resolve_preset(&builtin, "code").unwrap().expect_files, None);

        let t = table(
            "[parent]\npreamble = \"p\"\nexpect_files = [\"{reports}/summary.md\"]\n\n\
             [child]\nextends = \"parent\"\npreamble = \"c\"\n\n\
             [override_child]\nextends = \"parent\"\npreamble = \"o\"\nexpect_files = [\"out.bin\"]\n",
        );
        assert_eq!(
            resolve_preset(&t, "child").unwrap().expect_files,
            Some(vec!["{reports}/summary.md".to_string()]),
            "a child inherits its parent's expect_files"
        );
        assert_eq!(
            resolve_preset(&t, "override_child").unwrap().expect_files,
            Some(vec!["out.bin".to_string()]),
            "a child overrides it"
        );
    }

    #[test]
    fn a_verify_command_the_sub_agent_could_not_run_is_refused_at_resolve_time() {
        // A presets file can arrive with a repo you merely cloned. Anything the
        // bridge would run on the user's machine has to clear the same bar as what
        // the sub-agent is allowed to run in that repo.
        for bad in [
            "cargo build | tee out.txt",
            "cargo test > log.txt",
            "cargo test; rm -rf x",
            "curl http://example.com",
            "cargo publish",
            "powershell -Command \"ls\"",
            // Every segment is safe, but running a chain needs a shell we will not spawn.
            "cargo fmt --check && cargo test",
        ] {
            // A TOML literal string, so an embedded quote in the fixture is data.
            let t = table(&format!("[p]\npreamble = \"x\"\nverify = '{bad}'\n"));
            let e = resolve_preset(&t, "p").unwrap_err().to_string();
            assert!(e.contains("verify command"), "`{bad}` must be refused: {e}");
        }
        for good in ["cargo check --all-targets", "npm run check", "pytest -q"] {
            assert!(
                verify_command_is_safe(good),
                "`{good}` is an ordinary verification command"
            );
        }
    }

    #[test]
    fn a_typoed_key_is_rejected_rather_than_ignored() {
        // Silently dropping `expects_change` would disarm the punt nudge and look like
        // the nudge broke, not like the file did.
        let e = parse_presets("[code]\npreamble = \"x\"\nexpects_change = true\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("presets TOML"), "{e}");
    }

    #[test]
    fn env_table_parses() {
        let t = table(
            "[lane_a]\npreamble = \"x\"\n\
             env = { CARGO_TARGET_DIR = \"target/lane_a\", FOO = \"bar\" }\n",
        );
        let r = resolve_preset(&t, "lane_a").unwrap();
        assert_eq!(
            r.env.get("CARGO_TARGET_DIR").map(String::as_str),
            Some("target/lane_a")
        );
        assert_eq!(r.env.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn extends_merges_env_parent_first_child_wins_per_key() {
        let t = table(
            "[code]\npreamble = \"p\"\nenv = { A = \"parent-a\", B = \"parent-b\" }\n\n\
             [lane_a]\nextends = \"code\"\npreamble = \"c\"\nenv = { B = \"child-b\", C = \"child-c\" }\n",
        );
        let r = resolve_preset(&t, "lane_a").unwrap();
        assert_eq!(
            r.env.get("A").map(String::as_str),
            Some("parent-a"),
            "unmentioned in the child = inherited from the parent"
        );
        assert_eq!(
            r.env.get("B").map(String::as_str),
            Some("child-b"),
            "the nearest explicit answer wins, per key — not whole-table override"
        );
        assert_eq!(r.env.get("C").map(String::as_str), Some("child-c"));
    }

    #[test]
    fn env_values_get_the_same_path_substitution_as_the_preamble() {
        let t = table(
            "[lane_a]\npreamble = \"x\"\n\
             env = { CARGO_TARGET_DIR = \"{workspace}/target/lane_a\", NOTES = \"{reports}/n.md\" }\n",
        );
        let r = resolve_preset(&t, "lane_a").unwrap();
        let sub = substituted_env(&r.env, Path::new("C:/ws"), "C:/reports");
        assert_eq!(
            sub.get("CARGO_TARGET_DIR").map(String::as_str),
            Some("C:/ws/target/lane_a")
        );
        assert_eq!(
            sub.get("NOTES").map(String::as_str),
            Some("C:/reports/n.md")
        );
    }

    #[test]
    fn a_malformed_env_key_is_refused_naming_preset_file_and_key() {
        for bad in ["1START", "has-dash", "has space", ""] {
            let def = {
                let mut d = PresetDef::default();
                d.env.insert(bad.to_string(), "v".to_string());
                d
            };
            let e = validate_preset_env("lane_a", "some/file.toml", &def)
                .unwrap_err()
                .to_string();
            assert!(e.contains("preset `lane_a`"), "{e}");
            assert!(e.contains("some/file.toml"), "{e}");
            assert!(e.contains("valid name"), "`{bad}` should fail: {e}");
        }
    }

    #[test]
    fn reserved_env_names_are_refused_case_insensitively() {
        for reserved in [
            "PATH",
            "path",
            "Path",
            "PATHEXT",
            "ComSpec",
            "PSModulePath",
            "SystemRoot",
            "RUSTC_WRAPPER",
            "rustc_workspace_wrapper",
            "Cargo_Build_Rustc_Wrapper",
            "AGY_MCP_DELEGATED",
            "AGY_MCP_POLICY",
            "AGY_PERMISSIONS",
            "agy_cli_disable_auto_update",
            "AGY_anything_at_all",
        ] {
            let mut d = PresetDef::default();
            d.env.insert(reserved.to_string(), "v".to_string());
            let e = validate_preset_env("lane_a", "f.toml", &d)
                .unwrap_err()
                .to_string();
            assert!(
                e.contains("reserved"),
                "`{reserved}` should be reserved: {e}"
            );
        }
    }

    #[test]
    fn a_nul_or_newline_in_an_env_value_is_refused() {
        for bad_value in ["has\0nul", "has\nnewline", "has\rcr"] {
            let mut d = PresetDef::default();
            d.env.insert("OK_NAME".to_string(), bad_value.to_string());
            let e = validate_preset_env("lane_a", "f.toml", &d)
                .unwrap_err()
                .to_string();
            assert!(e.contains("NUL or"), "{e}");
        }
    }

    #[test]
    fn a_legal_env_name_passes_validation() {
        for good in ["CARGO_TARGET_DIR", "_leading_underscore", "a", "A1_b2"] {
            let mut d = PresetDef::default();
            d.env.insert(good.to_string(), "value".to_string());
            validate_preset_env("lane_a", "f.toml", &d)
                .unwrap_or_else(|e| panic!("`{good}` should be a legal env name: {e}"));
        }
    }

    #[test]
    fn a_bad_env_entry_in_a_layer_fails_load_table_before_merge() {
        // Exercises the actual call site (`load_table` validates per-layer), not just
        // the pure helper directly, so a regression that stops calling it is caught.
        let layer = table("[p]\npreamble = \"x\"\nenv = { PATH = \"evil\" }\n");
        for (name, def) in &layer {
            let e = validate_preset_env(name, "layer.toml", def);
            assert!(e.is_err());
        }
    }
}
