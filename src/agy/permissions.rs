use crate::agy::paths::data_dir;
use std::path::{Path, PathBuf};

fn merge_trusted_workspace(
    mut root: serde_json::Value,
    new_paths: &[&str],
) -> (serde_json::Value, bool) {
    let mut changed = false;
    if !root.is_object() {
        root = serde_json::json!({});
    }

    for &new_path in new_paths {
        let obj = root.as_object_mut().unwrap();
        let workspaces = obj
            .entry("trustedWorkspaces")
            .or_insert_with(|| serde_json::json!([]));

        if !workspaces.is_array() {
            *workspaces = serde_json::json!([]);
        }

        let arr = workspaces.as_array_mut().unwrap();

        let already_trusted = arr.iter().any(|v| {
            v.as_str()
                .map(|s| {
                    if cfg!(windows) {
                        s.eq_ignore_ascii_case(new_path)
                    } else {
                        s == new_path
                    }
                })
                .unwrap_or(false)
        });

        if !already_trusted {
            arr.push(serde_json::json!(new_path));
            changed = true;
        }

        let rule = format!("write_file({})", new_path);
        let perms = obj
            .entry("permissions")
            .or_insert_with(|| serde_json::json!({}));
        if !perms.is_object() {
            *perms = serde_json::json!({});
        }
        let allow = perms
            .as_object_mut()
            .unwrap()
            .entry("allow")
            .or_insert_with(|| serde_json::json!([]));
        if !allow.is_array() {
            *allow = serde_json::json!([]);
        }
        let allow_arr = allow.as_array_mut().unwrap();
        let already_allowed = allow_arr.iter().any(|v| {
            v.as_str()
                .map(|s| {
                    if cfg!(windows) {
                        s.eq_ignore_ascii_case(&rule)
                    } else {
                        s == rule
                    }
                })
                .unwrap_or(false)
        });

        if !already_allowed {
            allow_arr.push(serde_json::json!(rule));
            changed = true;
        }
    }

    (root, changed)
}

/// One argument: anything but whitespace and the shell constructs that make a line
/// mean more than it says. This character class is the ENTIRE safety property of the
/// regex grants below — agy full-matches a `regex:` rule against the **whole command
/// line** and does *not* decompose a chain first (both measured 2026-07-27, see
/// design-notes), so `cargo build; rm -rf x` is refused only because the `;` cannot
/// appear in anything a family pattern matches. Same boundary `split_command_chain`
/// draws, expressed in RE2.
const RE_ARG: &str = r#"[^\s;&|<>$()"'`]+"#;

/// `RE_ARG`, minus anything that starts with `-`. Used only for the npm-family
/// install verbs, where a global flag (`-g`, `--global`, `--location=global`) makes
/// the command a PATH install — the one thing this family must not pre-grant.
///
/// RE2 has no negative lookahead, so "any argument except these three literals"
/// cannot be written directly; enumerating its complement is possible but produced
/// a class nobody could audit at a glance. Requiring install-ish lines to carry **no
/// flags at all** is the same guarantee in a form that reads itself. It is
/// deliberately narrower than the policy: `npm ci --silent` simply falls off the
/// fast path, costs one recovery round-trip, and is then granted by
/// `denied_subcommand` — which is exactly what the regex ⊆ policy invariant buys.
const RE_PLAIN_ARG: &str = r#"[^-\s;&|<>$()"'`][^\s;&|<>$()"'`]*"#;

/// Optional leading directory, so a fully-qualified program classifies like its bare
/// name (agy runs the absolute path it just found via `where cargo`).
const RE_DIR: &str = r#"(?:[^\s;&|<>$()"'`]*[\\/])?"#;

/// `(?i)^[<dir>]<prog>[.exe][ <required first args>][ <arg>]*$`, RE2 (agy is Go), fully
/// anchored because agy requires a full match (measured: `regex:^Get-Random` does NOT
/// match `Get-Random -Maximum 5`).
fn command_pattern(progs: &str, first_args: Option<&str>) -> String {
    let first = match first_args {
        Some(a) => format!(r"\s+(?:{a})"),
        None => String::new(),
    };
    format!(r"(?i)^{RE_DIR}(?:{progs})(?:\.exe)?{first}(?:\s+{RE_ARG})*$")
}

/// The build/test/discovery families, as `regex:` rule targets.
///
/// agy 1.0.13 made "Always Approve" matching strict but kept an opt-in: a rule
/// prefixed `regex:` is matched as a pattern. Before that the curated list had to
/// name every **exact** command line — 40 strings that could never be complete,
/// because `command(cargo test)` does not match `cargo test --workspace` (measured
/// 2026-07-27: `command(git status)` did not match `git status --short --branch`).
///
/// INVARIANT: The regex pre-grant is a FAST PATH; `is_safe_command_segment` is the POLICY.
/// Every command line matched by a `verification_command_grants()` pattern must also be
/// accepted by `is_safe_command_segment`. The regex may be narrower, NEVER broader.
pub fn verification_command_grants() -> Vec<String> {
    vec![
        // Build/test/package managers: restricted first-argument alternations.
        command_pattern(
            "cargo",
            Some("build|test|check|clippy|fmt|tree|doc|bench|run|update|fix|clean|add|remove|metadata|locate-project|nextest|version"),
        ),
        // npm/yarn/pnpm: separate non-install verbs from install-ish verbs (install|i|ci|add)
        // where remaining arguments must not be global flags (-g, --global, --location=global).
        command_pattern(
            "npm|yarn|pnpm",
            Some("run|test|build|list|ls|why|outdated|audit|exec|dlx"),
        ),
        format!(
            r"(?i)^{RE_DIR}(?:npm|yarn|pnpm)(?:\.exe)?\s+(?:install|i|ci|add)(?:\s+{RE_PLAIN_ARG})*$"
        ),
        // npx stays fully allowed: it has no subcommand, and running a package from the
        // network is not distinguishable from npm install running postinstall scripts.
        command_pattern("npx", None),
        command_pattern("pytest", None),
        command_pattern(
            "mvn|gradle",
            Some("test|build|verify|compile|package|check|assemble|clean|install"),
        ),
        command_pattern("go", Some("test|build")),
        command_pattern("dotnet", Some("test")),
        command_pattern("python|python3", Some(r"-m\s+pytest")),
        // Syntax check only. In a TS/JS project this is the most frequent verification
        // command there is, and without it the run dies: `node --check` was soft-denied
        // and auto-recovery did not reach it, ending a run at 99s with zero edits
        // (measured 2026-08-01). Deliberately NOT the whole `node` family — `node app.js`
        // executes arbitrary JS, which is a different grant from "parse this file".
        // Split by tool because the spelling differs: node/bun take `--check`, deno
        // takes the `check` subcommand. One alternation covering both would admit
        // `node check foo`, which the policy refuses — fast path wider than policy.
        // Trailing arguments are `RE_PLAIN_ARG` (the npm-install treatment): a file to
        // parse never starts with `-`, and requiring that is how the pattern excludes
        // `node --check -e <code>` — which would otherwise ride in on the `--check`.
        format!(r"(?i)^{RE_DIR}(?:node|bun)(?:\.exe)?\s+--check(?:\s+{RE_PLAIN_ARG})*$"),
        format!(r"(?i)^{RE_DIR}deno(?:\.exe)?\s+(?:check|--check)(?:\s+{RE_PLAIN_ARG})*$"),
        // Toolchain discovery. The gate fires here far more often than on the build
        // itself — agy looks for the binary before running it (measured 2026-07-26) —
        // and a denial costs a whole recovery round-trip.
        command_pattern("where|which", None),
        // A version probe takes NO further arguments. The policy accepts `[bin, flag]`
        // — exactly two tokens — so `command_pattern`'s generic trailing-argument tail
        // put this pattern OUTSIDE the policy: `node --version extra` full-matched here
        // and was refused there (found 2026-08-13, present since the family shipped).
        // Hand-rolled because the builder always appends that tail.
        format!(
            r"(?i)^{RE_DIR}(?:node|python|python3|rustc|rustup|git|dotnet|java|deno|bun|go)(?:\.exe)?\s+(?:--version|-version|-V|version)$"
        ),
        // A bare env-var read, name-shaped only — no expression smuggling.
        r"(?i)^\$env:[A-Za-z0-9_]+$".to_string(),
    ]
    .into_iter()
    .map(|p| format!("regex:{p}"))
    .collect()
}

/// Grant entries **this bridge shipped in an earlier version** and has since narrowed.
///
/// The startup merge is append-only and permanent, so narrowing a pattern in
/// `verification_command_grants()` does nothing on a machine that already ran the wider
/// one — the old entry keeps matching and the new policy never gets a say. Measured
/// 2026-07-27: the pattern below was still live in `config.json` and full-matched
/// `cargo publish --dry-run`, which the current fast path and `denied_subcommand` both
/// refuse.
///
/// **Scope is deliberately narrow: only the exact bytes we wrote ourselves.** Not a sweep
/// of `regex:` grants, not the runtime auto-grant entries, not anything hand-added — a
/// startup task that deletes permissions it does not recognize is a far worse failure than
/// a stale pattern.
///
/// These are HISTORICAL LITERALS. Do not "clean them up" into `command_pattern(..)` calls:
/// the point is to match what an old binary wrote to disk, so they must not move when the
/// pattern builder or `RE_ARG` does.
const RETIRED_GRANTS: &[&str] = &[
    // Shipped until 2026-07-27: one family alternation with NO first-argument restriction,
    // so every subcommand rode along (`cargo publish`, `npm login`, `gradle deploy`).
    // Replaced by per-family patterns whose first argument is an explicit allow-list.
    r#"regex:(?i)^(?:[^\s;&|<>$()"'`]*[\\/])?(?:cargo|npm|npx|yarn|pnpm|pytest|mvn|gradle)(?:\.exe)?(?:\s+[^\s;&|<>$()"'`]+)*$"#,
    // Shipped until 2026-08-13: the version-probe family built with the generic
    // trailing-argument tail, so `node --version extra` full-matched it while the
    // policy's two-token `[bin, flag]` arm refused the same line.
    r#"regex:(?i)^(?:[^\s;&|<>$()"'`]*[\\/])?(?:node|python|python3|rustc|rustup|git|dotnet|java|deno|bun|go)(?:\.exe)?\s+(?:--version|-version|-V|version)(?:\s+[^\s;&|<>$()"'`]+)*$"#,
];

/// Drop any `RETIRED_GRANTS` entry from `userSettings.globalPermissionGrants.allow`.
/// Pure; returns whether anything was removed. Both the wrapped form we write
/// (`command(<rule>)`) and a bare `<rule>` are matched, on exact string equality only.
pub fn prune_retired_grants(mut root: serde_json::Value) -> (serde_json::Value, bool) {
    let Some(allow) = root
        .get_mut("userSettings")
        .and_then(|v| v.get_mut("globalPermissionGrants"))
        .and_then(|v| v.get_mut("allow"))
        .and_then(|v| v.as_array_mut())
    else {
        return (root, false);
    };

    let before = allow.len();
    allow.retain(|v| {
        let Some(s) = v.as_str() else { return true };
        !RETIRED_GRANTS
            .iter()
            .any(|r| s == *r || s == format!("command({r})"))
    });
    let removed = allow.len() != before;
    (root, removed)
}

/// How the bridge asks agy to treat tool permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// agy's headless gate stays on: un-granted tools are soft-denied and the
    /// bridge recovers what `auto_grant_rule` calls safe. The default.
    Gate,
    /// `--dangerously-skip-permissions`. Opt-in at SETUP time only
    /// (`claude mcp add agy … -e AGY_PERMISSIONS=trusted`), never per call: the
    /// point is that the human who wired the server up decided once, with the
    /// whole machine in view, instead of a model deciding per task.
    Trusted,
}

/// Pure parse of `AGY_PERMISSIONS`. `None` = not a value we recognize, which the
/// caller reports rather than silently reading as either mode.
pub fn parse_permission_mode(raw: &str) -> Option<PermissionMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "gate" | "default" => Some(PermissionMode::Gate),
        "trusted" => Some(PermissionMode::Trusted),
        _ => None,
    }
}

/// `AGY_PERMISSIONS` as a mode. An unrecognized value warns and keeps the gate —
/// a typo must never be the reason permissions came off.
pub fn permission_mode() -> PermissionMode {
    let Ok(raw) = std::env::var("AGY_PERMISSIONS") else {
        return PermissionMode::Gate;
    };
    match parse_permission_mode(&raw) {
        Some(m) => m,
        None => {
            eprintln!(
                "Warning: AGY_PERMISSIONS={raw:?} is not one of gate|trusted; keeping the permission gate on"
            );
            PermissionMode::Gate
        }
    }
}

pub fn merge_permission_grant_entries(
    mut root: serde_json::Value,
    entries: &[&str],
) -> (serde_json::Value, bool) {
    let mut changed = false;
    if !root.is_object() {
        root = serde_json::json!({});
    }

    let obj = root.as_object_mut().unwrap();
    let user_settings = obj
        .entry("userSettings")
        .or_insert_with(|| serde_json::json!({}));
    if !user_settings.is_object() {
        *user_settings = serde_json::json!({});
    }

    let user_obj = user_settings.as_object_mut().unwrap();
    let global_grants = user_obj
        .entry("globalPermissionGrants")
        .or_insert_with(|| serde_json::json!({}));
    if !global_grants.is_object() {
        *global_grants = serde_json::json!({});
    }

    let grants_obj = global_grants.as_object_mut().unwrap();
    let allow = grants_obj
        .entry("allow")
        .or_insert_with(|| serde_json::json!([]));
    if !allow.is_array() {
        *allow = serde_json::json!([]);
    }

    let allow_arr = allow.as_array_mut().unwrap();

    for &entry in entries {
        let already_present = allow_arr
            .iter()
            .any(|v| v.as_str().map(|s| s == entry).unwrap_or(false));
        if !already_present {
            allow_arr.push(serde_json::json!(entry));
            changed = true;
        }
    }

    (root, changed)
}

pub fn merge_global_permission_grants(
    root: serde_json::Value,
    commands: &[&str],
) -> (serde_json::Value, bool) {
    let formatted: Vec<String> = commands
        .iter()
        .map(|cmd| format!("command({cmd})"))
        .collect();
    let refs: Vec<&str> = formatted.iter().map(|s| s.as_str()).collect();
    merge_permission_grant_entries(root, &refs)
}

/// agy's shared `config.json` (holds `globalPermissionGrants`).
fn config_json_path() -> PathBuf {
    data_dir()
        .parent()
        .map(|p| p.join("config"))
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".gemini")
                .join("config")
        })
        .join("config.json")
}

/// ONE lock for every writer of `config.json` — startup's curated merge and the
/// mid-run soft-deny grant both rewrite the same file, so they must serialize
/// against each other (a per-function lock would not).
fn config_json_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Permanently add one verbatim grant entry (e.g. `command(cargo test)`,
/// `write_file(C:\proj)`) to config.json. Returns whether the file changed.
/// Never clobbers a config we can't parse (same rule as the MCP swap).
pub fn grant_permission_entry(entry: &str) -> bool {
    let config_file = config_json_path();
    let _guard = config_json_lock().lock().unwrap();

    let json_text = if config_file.exists() {
        match std::fs::read_to_string(&config_file) {
            Ok(t) => t,
            Err(_) => return false,
        }
    } else {
        "{}".to_string()
    };

    let Ok(root) = serde_json::from_str::<serde_json::Value>(&json_text) else {
        return false;
    };
    let (new_root, changed) = merge_permission_grant_entries(root, &[entry]);
    if changed {
        let Ok(new_text) = serde_json::to_string_pretty(&new_root) else {
            return false;
        };
        if let Some(parent) = config_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let temp_file = config_file.with_extension("json.tmp");
        if std::fs::write(&temp_file, new_text).is_err() {
            return false;
        }
        if std::fs::rename(&temp_file, &config_file).is_err() {
            return false;
        }
        true
    } else {
        false
    }
}

pub fn parse_denied_permission(stderr: &str) -> Option<String> {
    if !stderr.contains("auto-denied") {
        return None;
    }
    let tag = "a tool required the \"";
    let start = stderr.find(tag)? + tag.len();
    let rest = &stderr[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Split a command line into sequentially-chained segments, or None if it uses
/// any shell construct we refuse to reason about.
///
/// `;`, `&&` and `||` merely *sequence* independent command lines, so a chain is
/// grantable exactly when every segment is. Everything else changes where output
/// goes or what else runs — pipes, redirects, command substitution, a
/// backgrounding bare `&` — and the family check below cannot see through those,
/// so they stay a hard reject. This is what makes the observed
/// `$env:Path; where.exe cargo` (agy hunting for the toolchain before the build
/// it was told to run) recoverable at all: the whole line used to be rejected on
/// the `;` alone. See design-notes.
pub(crate) fn split_command_chain(target: &str) -> Option<Vec<&str>> {
    let b = target.as_bytes();
    let mut segs: Vec<&str> = Vec::new();
    let (mut start, mut i) = (0usize, 0usize);
    while i < b.len() {
        let c = b[i];
        match c {
            b'`' | b'\n' | b'\r' | b'<' | b'>' => return None,
            b'$' if b.get(i + 1) == Some(&b'(') => return None,
            b';' => {
                segs.push(&target[start..i]);
                i += 1;
                start = i;
            }
            b'&' | b'|' => {
                // `&&` / `||` sequence; a bare `&` (background) or `|` (pipe) does not.
                if b.get(i + 1) == Some(&c) {
                    segs.push(&target[start..i]);
                    i += 2;
                    start = i;
                } else {
                    return None;
                }
            }
            _ => i += 1,
        }
    }
    segs.push(&target[start..]);
    if segs.iter().any(|s| s.trim().is_empty()) {
        return None; // trailing/empty separator — don't guess what was meant
    }
    Some(segs)
}

/// Read-only probes agy reaches for when it goes looking for a toolchain. These
/// are separate from the build/test family because they are *discovery*, and
/// discovery is where the headless gate actually fires (measured 2026-07-26).
/// Kept to an explicit binary list — `<anything> --version` would still be
/// "execute an arbitrary binary".
const DISCOVERY_BINS: &[&str] = &[
    "node", "python", "python3", "rustc", "rustup", "git", "dotnet", "java", "deno", "bun", "go",
];

fn is_version_flag(t: &str) -> bool {
    matches!(t, "--version" | "-version" | "-V" | "-v" | "version")
}

/// Program name of a command token: directory stripped, a trailing `.exe` removed.
/// Pure so the family classification can be unit-tested. Deliberately name-only —
/// the guard's threat model is "agy picked a command shape we can't reason about",
/// not "a hostile binary was planted on the user's own machine" (agy already runs
/// with the user's file/shell access; see AGENTS.md's trusted-prompts-only rule).
pub(crate) fn command_basename(tok: &str) -> &str {
    let base = tok.rsplit(['/', '\\']).next().unwrap_or(tok);
    if let Some(cut) = base.len().checked_sub(4) {
        if base.is_char_boundary(cut) && base[cut..].eq_ignore_ascii_case(".exe") {
            return &base[..cut];
        }
    }
    base
}

/// Checks if a program subcommand or argument list is disallowed by runtime policy.
///
/// A build/test grant ALREADY means executing arbitrary code from the checkout
/// (`cargo test` runs the repo's own code). So the grants are not about preventing
/// code execution. They are about preventing effects that reach OUTSIDE the checkout:
/// publishing to a registry, touching registry credentials, or installing a binary
/// onto PATH. Those subcommands — and only those — are refused.
fn denied_subcommand(prog: &str, args: &[&str]) -> bool {
    let sub = match args.first() {
        Some(&s) => s,
        None => return false,
    };
    match prog {
        "cargo" => matches!(
            sub,
            "install" | "uninstall" | "publish" | "login" | "logout" | "owner" | "yank" | "token"
        ),
        "npm" | "yarn" | "pnpm" => {
            if matches!(
                sub,
                "publish"
                    | "unpublish"
                    | "login"
                    | "logout"
                    | "adduser"
                    | "token"
                    | "owner"
                    | "access"
                    | "deprecate"
                    | "dist-tag"
            ) {
                return true;
            }
            if matches!(sub, "install" | "add" | "i") {
                return args[1..]
                    .iter()
                    .any(|&arg| arg == "-g" || arg == "--global" || arg == "--location=global");
            }
            false
        }
        "mvn" | "gradle" => matches!(sub, "deploy" | "release" | "publish"),
        _ => false,
    }
}

/// Is one segment of a command chain safe to auto-grant? The build/test family
/// minus its outward-facing subcommands (`denied_subcommand`), a read-only
/// discovery probe, or a PowerShell env-var read. This is the POLICY — the
/// `regex:` pre-grants above are only a fast path onto it, never wider.
/// `-e`/`--eval`/`-p`/`--print` hand the runtime a program on the command line, so a
/// syntax-check grant must not carry one — `node --check -e <code>` would otherwise ride
/// in on the `--check`. node rejects that combination itself today, but a grant is
/// permanent and the CLI is not ours, so the guard does not rest on that staying true.
fn has_inline_runner(args: &[&str]) -> bool {
    args.iter()
        .any(|a| matches!(*a, "-e" | "--eval" | "-p" | "--print"))
}

pub(crate) fn is_safe_command_segment(seg: &str) -> bool {
    let seg = seg.trim();
    // `$env:Path` — a bare env-var read, no side effects. Name-shaped only.
    if let Some(name) = seg.strip_prefix("$env:") {
        return !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    }
    let mut tokens: Vec<&str> = seg.split_whitespace().collect();
    // The program may arrive fully qualified — agy reaches for the absolute path it
    // just found via `where cargo`, and the family match below is on the program
    // NAME. Observed killing a run 2026-07-26: `C:\Users\x\.cargo\bin\cargo.exe test`
    // was refused while a bare `cargo test` would have been granted.
    if let Some(first) = tokens.first_mut() {
        *first = command_basename(first);
    }
    match tokens.as_slice() {
        [] => false,
        [prog @ ("cargo" | "npm" | "yarn" | "pnpm" | "mvn" | "gradle"), args @ ..] => {
            !denied_subcommand(prog, args)
        }
        ["npx", ..]
        | ["pytest", ..]
        | ["go", "test", ..]
        | ["go", "build", ..]
        | ["dotnet", "test", ..] => true,
        ["python", "-m", "pytest", ..] => true,
        ["node" | "bun", "--check", rest @ ..] => !has_inline_runner(rest),
        ["deno", "check" | "--check", rest @ ..] => !has_inline_runner(rest),
        // Discovery: locate a binary on PATH, or ask a known tool its version.
        ["where" | "where.exe" | "which" | "which.exe", ..] => true,
        [bin, flag] => DISCOVERY_BINS.contains(bin) && is_version_flag(flag),
        _ => false,
    }
}

pub fn auto_grant_rule(kind: &str, target: &str, workspace: &Path) -> Option<String> {
    if kind == "command" {
        let segs = split_command_chain(target)?;
        if segs.iter().all(|s| is_safe_command_segment(s)) {
            // Grants are exact-string matches against what agy asked for, so the
            // rule carries the WHOLE line, chain and all — not the segments.
            Some(format!("command({target})"))
        } else {
            None
        }
    } else if kind == "read_url" {
        // agy 1.1.28 changed URL fetching from always-allowed to ask-first, which made
        // `preset=research` die at the gate with no recovery: this function returned
        // None for every kind it did not know, so a soft-denied URL read ended the run.
        // Measured 2026-09-09 (agy 1.1.28, toolPermission=request-review): the kind is
        // `read_url` and the grant agy names is `read_url(<target>)`.
        //
        // Granted for the EXACT url the sub-agent asked for, nothing wider. A grant is
        // an exact-string match and permanent, so a scheme check plus "no credentials
        // in the authority" is the whole gate: it keeps a `file://` or a
        // `https://user:token@host` out of the permanent allow list, while an ordinary
        // page read — which this agent could already reach through `search_web` — costs
        // one round-trip instead of the run.
        let t = target.trim();
        let looks_like_web = t.starts_with("http://") || t.starts_with("https://");
        let authority = t
            .split_once("//")
            .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
            .unwrap_or("");
        if looks_like_web && !authority.contains('@') && !authority.is_empty() {
            Some(format!("read_url({t})"))
        } else {
            None
        }
    } else if kind == "write_file" || kind == "read_file" {
        // Windows paths are case-insensitive and agy mixes `/` and `\`, so compare
        // on a normalized, lowercased form rather than Path::starts_with.
        let in_workspace = if cfg!(windows) {
            let norm = |s: &str| s.replace('/', "\\").trim_end_matches('\\').to_lowercase();
            let t = norm(target);
            let w = norm(&workspace.to_string_lossy());
            !w.is_empty() && (t == w || t.starts_with(&format!("{w}\\")))
        } else {
            Path::new(target).starts_with(workspace)
        };
        if in_workspace {
            Some(format!("{kind}({})", workspace.display()))
        } else {
            None
        }
    } else {
        None
    }
}

#[derive(Debug)]
pub struct DeniedError {
    pub kind: String,
    pub tool: String,
    pub target: String,
    pub conv_id: Option<String>,
}

impl std::fmt::Display for DeniedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tool = if self.tool.is_empty() {
            "tool"
        } else {
            &self.tool
        };
        let target = if self.target.is_empty() {
            "target"
        } else {
            &self.target
        };
        write!(
            f,
            "a tool was soft-denied in headless mode (agy 1.1.3+ permission gate): {tool}({target}) \
             needs the \"{}\" permission and is outside what this bridge auto-grants. File edits \
             made before the denial ARE on disk.",
            self.kind
        )?;
        // Everything a human needs to settle this in one decision. The rule has to be
        // the whole line verbatim: measured 2026-08-14, neither argument-prefix
        // matching nor agy's documented `command(a (b|c))` shorthand admits a command
        // with extra arguments, so it cannot be generalised on their behalf either.
        if !self.kind.is_empty() && !self.target.is_empty() {
            write!(
                f,
                " TO ALLOW IT, add exactly `{}({})` to userSettings.globalPermissionGrants.allow \
                 in ~/.gemini/config/config.json — that list is the operator's own record, so it \
                 is their call and the bridge never widens it for them.",
                self.kind, self.target
            )?;
        }
        match &self.conv_id {
            Some(id) => write!(
                f,
                " THEN RESUME, don't restart: agy_sub(conversation_id=\"{id}\", \
                 prompt=\"Continue the task from where it stopped.\") keeps the work already done.",
            ),
            None => write!(f, " Otherwise run that step yourself."),
        }
    }
}

impl std::error::Error for DeniedError {}

fn process_config_json(json_text: &str, commands: &[&str]) -> Result<Option<String>, ()> {
    let root = serde_json::from_str::<serde_json::Value>(json_text).map_err(|_| ())?;
    // Retire superseded patterns of ours BEFORE adding the current set: otherwise a
    // narrowed grant is dead on arrival on any machine that already has the wide one.
    let (root, pruned) = prune_retired_grants(root);
    let (new_root, merged) = merge_global_permission_grants(root, commands);
    if pruned || merged {
        Ok(Some(
            serde_json::to_string_pretty(&new_root).map_err(|_| ())?,
        ))
    } else {
        Ok(None)
    }
}

pub fn ensure_verification_commands_allowed() -> anyhow::Result<bool> {
    let config_file = config_json_path();
    let _guard = config_json_lock().lock().unwrap();

    let json_text = if config_file.exists() {
        match std::fs::read_to_string(&config_file) {
            Ok(t) => t,
            Err(_) => return Ok(false),
        }
    } else {
        "{}".to_string()
    };

    let grants = verification_command_grants();
    let refs: Vec<&str> = grants.iter().map(|s| s.as_str()).collect();
    match process_config_json(&json_text, &refs) {
        Ok(Some(new_text)) => {
            if let Some(parent) = config_file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let temp_file = config_file.with_extension("json.tmp");
            std::fs::write(&temp_file, new_text)?;
            std::fs::rename(&temp_file, &config_file)?;
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(_) => Ok(false),
    }
}

fn process_settings_json(json_text: &str, new_paths: &[&str]) -> Result<Option<String>, ()> {
    let root = serde_json::from_str::<serde_json::Value>(json_text).map_err(|_| ())?;
    let (new_root, changed) = merge_trusted_workspace(root, new_paths);
    if changed {
        Ok(Some(
            serde_json::to_string_pretty(&new_root).map_err(|_| ())?,
        ))
    } else {
        Ok(None)
    }
}

/// Strip Windows extended-length prefixes from a canonicalized path string.
/// `\\?\C:\x` -> `C:\x`; `\\?\UNC\server\share` -> `\\server\share`; otherwise unchanged.
pub fn strip_extended_prefix(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{}", rest)
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

pub fn ensure_workspaces_trusted(workspaces: &[&Path]) -> anyhow::Result<bool> {
    if workspaces.is_empty() {
        return Ok(false);
    }

    let mut ws_strs = Vec::with_capacity(workspaces.len());
    for workspace in workspaces {
        let mut ws_str = workspace.to_string_lossy().into_owned();
        if let Ok(canon) = workspace.canonicalize() {
            ws_str = canon.to_string_lossy().into_owned();
            if cfg!(windows) {
                ws_str = strip_extended_prefix(&ws_str);
            }
        }
        ws_strs.push(ws_str);
    }

    let ws_strs_ref: Vec<&str> = ws_strs.iter().map(|s| s.as_str()).collect();

    let settings_file = data_dir().join("settings.json");

    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap();

    let json_text = if settings_file.exists() {
        match std::fs::read_to_string(&settings_file) {
            Ok(t) => t,
            Err(_) => return Ok(false),
        }
    } else {
        "{}".to_string()
    };

    match process_settings_json(&json_text, &ws_strs_ref) {
        Ok(Some(new_text)) => {
            if let Some(parent) = settings_file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let temp_file = settings_file.with_extension("json.tmp");
            std::fs::write(&temp_file, new_text)?;
            std::fs::rename(&temp_file, &settings_file)?;
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A denial is where the bridge deliberately stops and hands the decision back, so
    // the message has to carry everything that decision needs: the exact rule (grants
    // are whole-line matches, so it cannot be generalised) and the resume that keeps
    // the work already done. Without both, the reader has to go and reconstruct them.
    #[test]
    fn a_denial_carries_the_grant_line_and_the_resume() {
        let msg = DeniedError {
            kind: "command".to_string(),
            tool: "run_command".to_string(),
            target: "git push --dry-run".to_string(),
            conv_id: Some("abc-123".to_string()),
        }
        .to_string();
        assert!(
            msg.contains("command(git push --dry-run)"),
            "the grant line must be pasteable verbatim: {msg}"
        );
        assert!(
            msg.contains("globalPermissionGrants.allow"),
            "and it must say where it goes: {msg}"
        );
        assert!(
            msg.contains("conversation_id=\"abc-123\""),
            "and how to continue instead of restarting: {msg}"
        );
        // No conversation id (denied before the conv dir was known) must not print a
        // resume line naming nothing.
        let no_conv = DeniedError {
            kind: "command".to_string(),
            tool: "run_command".to_string(),
            target: "git push".to_string(),
            conv_id: None,
        }
        .to_string();
        assert!(!no_conv.contains("conversation_id"), "{no_conv}");
        assert!(no_conv.contains("yourself"), "{no_conv}");
    }

    #[test]
    fn test_process_settings_json() {
        // 未登録パス → 追加され、かつ未知キーが保持される
        let json = r#"{
  "colorScheme": "tokyo night",
  "trustedWorkspaces": [
    "G:\\sandbox"
  ]
}"#;
        let res = process_settings_json(json, &["C:\\new\\dir"])
            .unwrap()
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&res).unwrap();
        assert_eq!(parsed["colorScheme"], "tokyo night");
        assert_eq!(parsed["trustedWorkspaces"][1], "C:\\new\\dir");
        // headless write grant merged alongside the trust entry
        assert_eq!(
            parsed["permissions"]["allow"][0],
            "write_file(C:\\new\\dir)"
        );

        // 既に含まれるパス → 配列不変 (Noneを返す)
        let res2 = process_settings_json(&res, &["C:\\new\\dir"]).unwrap();
        assert!(res2.is_none());

        // Windows での大文字小文字違い → 重複追加しない
        if cfg!(windows) {
            let res3 = process_settings_json(&res, &["c:\\NEW\\dir"]).unwrap();
            assert!(res3.is_none());
        }

        // 壊れた JSON → 上書きしない (Errを返す)
        let broken = r#"{ "colorScheme": "tokyo "#;
        assert!(process_settings_json(broken, &["C:\\new"]).is_err());
    }

    #[test]
    fn test_merge_global_permission_grants_empty() {
        let root = serde_json::json!({});
        let cmds = &["cargo build", "cargo test"];
        let (merged, changed) = merge_global_permission_grants(root, cmds);
        assert!(changed);
        let allow = merged["userSettings"]["globalPermissionGrants"]["allow"]
            .as_array()
            .unwrap();
        assert_eq!(
            allow,
            &vec![
                serde_json::json!("command(cargo build)"),
                serde_json::json!("command(cargo test)")
            ]
        );
    }

    #[test]
    fn test_merge_global_permission_grants_dedupe_and_preserve() {
        let input = serde_json::json!({
            "userSettings": {
                "remoteControlHostname": "example.com",
                "globalPermissionGrants": {
                    "allow": [
                        "command(cargo build)"
                    ]
                }
            }
        });
        let cmds = &["cargo build", "cargo test"];
        let (merged, changed) = merge_global_permission_grants(input, cmds);
        assert!(changed);
        assert_eq!(
            merged["userSettings"]["remoteControlHostname"],
            "example.com"
        );
        let allow = merged["userSettings"]["globalPermissionGrants"]["allow"]
            .as_array()
            .unwrap();
        assert_eq!(
            allow,
            &vec![
                serde_json::json!("command(cargo build)"),
                serde_json::json!("command(cargo test)")
            ]
        );

        // Submitting same commands again results in no change
        let (merged2, changed2) = merge_global_permission_grants(merged, cmds);
        assert!(!changed2);
        assert_eq!(
            merged2["userSettings"]["remoteControlHostname"],
            "example.com"
        );
    }

    /// Does any pre-granted family pattern accept this command line? Rust's `regex`
    /// is RE2-syntax like Go's, and every pattern is `^…$`-anchored, so `is_match`
    /// here means the same thing as agy's full match.
    fn granted(line: &str) -> bool {
        verification_command_grants().iter().any(|rule| {
            let pat = rule.strip_prefix("regex:").expect("rule is a regex rule");
            regex::Regex::new(pat)
                .expect("pattern compiles")
                .is_match(line)
        })
    }

    #[test]
    fn retired_grants_are_removed_and_only_them() {
        let retired = RETIRED_GRANTS[0];
        let root = serde_json::json!({
            "userSettings": {
                "remoteControlHostname": "example.com",
                "globalPermissionGrants": { "allow": [
                    "command(git status)",
                    format!("command({retired})"),
                    retired,
                    // A hand-added regex grant we did not write: must survive untouched.
                    "command(regex:(?i)^ruff check.*$)",
                ]}
            }
        });
        let (pruned, changed) = prune_retired_grants(root);
        assert!(changed);
        let allow = pruned["userSettings"]["globalPermissionGrants"]["allow"]
            .as_array()
            .unwrap();
        assert_eq!(
            allow,
            &vec![
                serde_json::json!("command(git status)"),
                serde_json::json!("command(regex:(?i)^ruff check.*$)"),
            ]
        );
        // Idempotent, and a config without the section is left alone rather than created.
        let (again, changed2) = prune_retired_grants(pruned);
        assert!(!changed2);
        assert_eq!(
            again["userSettings"]["remoteControlHostname"],
            "example.com"
        );
        assert!(!prune_retired_grants(serde_json::json!({})).1);
    }

    #[test]
    fn retired_grants_are_gone_from_the_current_set_and_were_worth_retiring() {
        // A retired entry that is still shipped would be pruned and re-added on every
        // startup — an infinite rewrite, and the narrowing would never land.
        for r in RETIRED_GRANTS {
            assert!(
                !verification_command_grants().iter().any(|g| g == r),
                "still shipped: {r}"
            );
        }
        // Why it was retired: it admitted exactly what the policy now refuses. Pinning
        // this keeps the literal honest — a typo'd retired string would match nothing.
        let pat = RETIRED_GRANTS[0].strip_prefix("regex:").unwrap();
        let re = regex::Regex::new(pat).expect("retired pattern compiles");
        assert!(re.is_match("cargo publish --dry-run"));
        assert!(re.is_match("npm login"));
        assert!(!granted("cargo publish --dry-run"));
        assert!(!granted("npm login"));

        // Same pin for the retired version-probe family: it admitted a trailing
        // argument, which the two-token discovery arm of the policy refuses.
        let ver = RETIRED_GRANTS[1].strip_prefix("regex:").unwrap();
        let re = regex::Regex::new(ver).expect("retired pattern compiles");
        assert!(re.is_match("node --version extra"));
        assert!(re.is_match("rustc --version"));
        assert!(!granted("node --version extra"));
        // The narrowed replacement still covers the probe agy actually issues.
        assert!(granted("rustc --version"));
    }

    #[test]
    fn grant_patterns_are_anchored() {
        // Anchoring is not decoration: agy matches the pattern against the WHOLE
        // command line, so an unanchored pattern would accept any line CONTAINING a
        // safe-looking command.
        for rule in verification_command_grants() {
            let pat = rule.strip_prefix("regex:").unwrap().to_string();
            assert!(pat.starts_with("(?i)^"), "not anchored at start: {pat}");
            assert!(pat.ends_with('$'), "not anchored at end: {pat}");
        }
    }

    #[test]
    fn grant_patterns_cover_the_family_and_stop_at_shell_constructs() {
        // The whole family, with arbitrary arguments — this is what the 40 exact
        // strings could not do (`command(cargo test)` never matched
        // `cargo test --workspace`, measured 2026-07-27).
        assert!(granted("cargo build"));
        assert!(granted("cargo test --workspace -- --nocapture"));
        assert!(granted("cargo test --test integration_tests test_f5_"));
        assert!(granted("npm run build"));
        assert!(granted("npx tsc --noEmit"));
        assert!(granted("pytest -k something"));
        assert!(granted("go test ./..."));
        assert!(granted("dotnet test --no-build"));
        assert!(granted("python -m pytest tests/"));
        assert!(granted("node --check cockpit/ui/main.js"));
        assert!(granted("deno check src/index.ts"));
        assert!(granted("bun --check src/index.js"));
        // Fully qualified, as agy issues it after `where cargo` (killed a run 2026-07-26).
        assert!(granted(
            r"C:\Users\x\.cargo\bin\cargo.exe test --bin agy-mcp"
        ));
        assert!(granted("/usr/local/bin/npm run build"));
        assert!(granted(r"C:\nodejs\node.exe --check app.js"));
        assert!(!granted(r"C:\Program Files\nodejs\node.exe --check app.js"));
        // Discovery.
        assert!(granted("where cargo"));
        assert!(granted("which node"));
        assert!(granted("rustc --version"));
        assert!(granted("go version"));
        assert!(granted("$env:Path"));

        // Shell constructs: a chain is NOT decomposed by agy before matching, so the
        // pattern itself has to refuse the line. This is the assertion that keeps
        // `cargo build; rm -rf x` denied.
        assert!(!granted("cargo build; rm -rf x"));
        assert!(!granted("cargo build && rm -rf x"));
        assert!(!granted("cargo build || rm -rf x"));
        assert!(!granted("cargo build | tee out.txt"));
        assert!(!granted("cargo build > out.txt"));
        assert!(!granted("cargo build $(rm -rf x)"));
        assert!(!granted("cargo build `rm -rf x`"));
        assert!(!granted("$env:Path; where.exe cargo"));

        // Outside the family, qualified or not.
        assert!(!granted("git status"));
        assert!(!granted("rm -rf /"));
        assert!(!granted("go run main.go"));
        assert!(!granted("dotnet publish"));
        assert!(!granted("python -c print(1)"));
        assert!(!granted(r"C:\tools\python.exe -c print(1)"));
        assert!(!granted("node app.js"));
        assert!(!granted("node -e console.log(1)"));
        // An inline runner must not ride in behind the syntax check.
        assert!(!granted("node --check -e code"));
        assert!(!granted("deno check --eval code"));
        assert!(!granted("node check app.js"));
        assert!(!granted("bun check app.js"));
        assert!(!granted("deno run -A app.ts"));
        assert!(!granted("bun run app.js"));
        assert!(!granted("somebinary --version"));
        assert!(!granted("$env:Path.Substring(0)"));
        // A safe command is not safe as a substring of something else.
        assert!(!granted("echo cargo build"));
    }

    #[test]
    fn test_parse_permission_mode() {
        assert_eq!(
            parse_permission_mode("trusted"),
            Some(PermissionMode::Trusted)
        );
        assert_eq!(
            parse_permission_mode(" TRUSTED "),
            Some(PermissionMode::Trusted)
        );
        assert_eq!(parse_permission_mode(""), Some(PermissionMode::Gate));
        assert_eq!(parse_permission_mode("gate"), Some(PermissionMode::Gate));
        // Unrecognized is NOT "close enough to trusted" — the caller keeps the gate.
        assert_eq!(parse_permission_mode("yes"), None);
        assert_eq!(parse_permission_mode("trusted!"), None);
    }

    #[test]
    fn test_strip_extended_prefix() {
        assert_eq!(strip_extended_prefix(r"\\?\C:\proj"), r"C:\proj");
        assert_eq!(
            strip_extended_prefix(r"\\?\UNC\server\share\proj"),
            r"\\server\share\proj"
        );
        assert_eq!(strip_extended_prefix(r"C:\proj"), r"C:\proj");
        assert_eq!(strip_extended_prefix(""), "");
    }

    #[test]
    fn test_parse_denied_permission() {
        let stderr = "jetski: no output produced — a tool required the \"command\" permission that headless mode cannot prompt for, so it was auto-denied. Add an allow-rule under permissions.allow in settings.json (e.g. command(<target>)). Alternatively, re-run with --dangerously-skip-permissions to auto-approve all tools.";
        assert_eq!(parse_denied_permission(stderr), Some("command".to_string()));
        assert_eq!(parse_denied_permission("some other error message"), None);
    }

    /// Measured 2026-09-09: agy 1.1.28 flipped URL fetching to ask-first, and the
    /// denial names the kind `read_url`. Before this arm existed, every kind this
    /// function did not know returned None, so a research run died at the gate with
    /// no auto-recovery at all.
    #[test]
    fn a_url_read_is_grantable_for_that_exact_url_and_nothing_wider() {
        let ws = Path::new(r"C:\ws");
        assert_eq!(
            auto_grant_rule("read_url", "https://example.com/docs#a", ws).as_deref(),
            Some("read_url(https://example.com/docs#a)")
        );
        assert_eq!(
            auto_grant_rule("read_url", "http://localhost:8080/x", ws).as_deref(),
            Some("read_url(http://localhost:8080/x)")
        );
        // Not a web fetch, or carrying credentials into a permanent allow entry.
        assert_eq!(
            auto_grant_rule("read_url", "file:///C:/secrets.txt", ws),
            None
        );
        assert_eq!(
            auto_grant_rule("read_url", "https://user:token@example.com/x", ws),
            None
        );
        assert_eq!(auto_grant_rule("read_url", "example.com", ws), None);
        assert_eq!(auto_grant_rule("read_url", "", ws), None);
    }

    #[test]
    fn test_auto_grant_rule() {
        let ws = Path::new(if cfg!(windows) {
            r"C:\project"
        } else {
            "/project"
        });
        assert_eq!(
            auto_grant_rule("command", "cargo --version", ws),
            Some("command(cargo --version)".to_string())
        );
        assert_eq!(
            auto_grant_rule("command", "cargo build; rm -rf x", ws),
            None
        );
        assert_eq!(auto_grant_rule("command", "git push", ws), None);

        // Chains: grantable only when EVERY segment is. The rule carries the
        // whole line because that is the string agy asked for.
        assert_eq!(
            auto_grant_rule("command", "$env:Path; where.exe cargo", ws),
            Some("command($env:Path; where.exe cargo)".to_string())
        );
        assert_eq!(
            auto_grant_rule("command", "cargo fmt && cargo test", ws),
            Some("command(cargo fmt && cargo test)".to_string())
        );
        // A fully-qualified program classifies like its bare name — agy uses the
        // absolute path it just discovered (observed killing a run 2026-07-26).
        assert_eq!(
            auto_grant_rule("command", r"C:\Users\x\.cargo\bin\cargo.exe test", ws),
            Some(r"command(C:\Users\x\.cargo\bin\cargo.exe test)".to_string())
        );
        assert_eq!(
            auto_grant_rule("command", "/usr/local/bin/npm run build", ws),
            Some("command(/usr/local/bin/npm run build)".to_string())
        );
        assert_eq!(
            auto_grant_rule("command", "cargo.exe --version", ws),
            Some("command(cargo.exe --version)".to_string())
        );
        // Normalization must not widen the family: a non-family program stays out,
        // fully-qualified or not.
        assert_eq!(
            auto_grant_rule("command", r"C:\bin\rustup.exe update", ws),
            None
        );
        assert_eq!(
            auto_grant_rule("command", r"C:\tools\python.exe -c print(1)", ws),
            None
        );
        // One unsafe segment poisons the chain (the original regression guard).
        assert_eq!(
            auto_grant_rule("command", "cargo build || rm -rf x", ws),
            None
        );
        assert_eq!(
            auto_grant_rule("command", "cargo build & git push", ws),
            None
        );
        // Constructs we refuse to reason about, even with a safe-looking family.
        assert_eq!(
            auto_grant_rule("command", "cargo build | tee out.txt", ws),
            None
        );
        assert_eq!(
            auto_grant_rule("command", "cargo build > out.txt", ws),
            None
        );
        assert_eq!(
            auto_grant_rule("command", "cargo build $(rm -rf x)", ws),
            None
        );
        assert_eq!(
            auto_grant_rule("command", "cargo build `rm -rf x`", ws),
            None
        );
        assert_eq!(auto_grant_rule("command", "cargo build;", ws), None);

        // Discovery probes: PATH lookups and version reads on known tools only.
        assert!(auto_grant_rule("command", "where.exe cargo", ws).is_some());
        assert!(auto_grant_rule("command", "which node", ws).is_some());
        assert!(auto_grant_rule("command", "rustc --version", ws).is_some());
        assert!(auto_grant_rule("command", "go version", ws).is_some());
        assert_eq!(auto_grant_rule("command", "somebinary --version", ws), None);
        assert_eq!(auto_grant_rule("command", "git status", ws), None);
        // Env reads are name-shaped only — no expression smuggling.
        assert!(auto_grant_rule("command", "$env:PATH", ws).is_some());
        assert_eq!(
            auto_grant_rule("command", "$env:PATH.Substring(0)", ws),
            None
        );

        let in_file = if cfg!(windows) {
            r"C:\project\src\main.rs"
        } else {
            "/project/src/main.rs"
        };
        let out_file = if cfg!(windows) {
            r"C:\other\src\main.rs"
        } else {
            "/other/src/main.rs"
        };

        assert_eq!(
            auto_grant_rule("write_file", in_file, ws),
            Some(format!("write_file({})", ws.display()))
        );
        assert_eq!(auto_grant_rule("write_file", out_file, ws), None);

        // Windows: agy mixes separators and case — both must still match.
        if cfg!(windows) {
            assert_eq!(
                auto_grant_rule("write_file", "c:/PROJECT/src/main.rs", ws),
                Some(format!("write_file({})", ws.display()))
            );
            // A sibling dir sharing the workspace's prefix is NOT inside it.
            assert_eq!(
                auto_grant_rule("write_file", r"C:\project-other\x.rs", ws),
                None
            );
        }
        // An unknown permission kind is never auto-granted.
        assert_eq!(auto_grant_rule("read_url", "api.github.com", ws), None);
    }

    #[test]
    fn regex_grants_never_exceed_the_runtime_policy() {
        let regexes: Vec<regex::Regex> = verification_command_grants()
            .iter()
            .map(|rule| {
                let pat = rule.strip_prefix("regex:").expect("rule is a regex rule");
                regex::Regex::new(pat).expect("pattern compiles")
            })
            .collect();

        let pattern_matches = |line: &str| regexes.iter().any(|re| re.is_match(line));

        let corpus = &[
            "cargo build",
            "cargo test --workspace --quiet",
            r"C:\Users\x\.cargo\bin\cargo.exe test",
            "cargo tree --depth 1",
            "cargo install --git https://x/y",
            "cargo publish",
            "cargo yank --vers 1.0",
            "npm ci",
            "npm install",
            "npm install -g typescript",
            "npm install --global typescript",
            "npm publish",
            "npm login",
            "yarn add react",
            "pnpm i",
            "npx tsc --noEmit",
            "mvn test",
            "mvn deploy",
            "gradle build",
            "go test ./...",
            "dotnet test",
            "python -m pytest -q",
            "where cargo",
            "rustc --version",
            // The witness for the retired version-probe pattern: a trailing argument
            // took it off the policy's two-token discovery arm.
            "node --version extra",
            "$env:PATHEXT",
            "node --check cockpit/ui/main.js",
            "deno check src/index.ts",
            "bun --check src/index.js",
            "node check main.js",
            "bun check main.js",
            "node --check -e code",
            "node app.js",
            "node -e console.log(1)",
            "deno run -A app.ts",
            "cargo build; rm -rf x",
            "cargo build | tee log",
            "Get-ChildItem Env:PATHEXT",
        ];

        // 1. Containment invariant: pattern_matches(line) => is_safe_command_segment(line)
        for &line in corpus {
            if pattern_matches(line) {
                assert!(
                    is_safe_command_segment(line),
                    "Pattern matched '{line}', but is_safe_command_segment denied it! Invariant violated."
                );
            }
        }

        // 2. Explicitly assert NO pattern matches dangerous commands / chained constructs
        let must_not_match = &[
            "cargo install --git https://x/y",
            "cargo publish",
            "npm publish",
            "npm login",
            "npm install -g typescript",
            "mvn deploy",
            "node app.js",
            "node -e console.log(1)",
            "node check main.js",
            "bun check main.js",
            "deno run -A app.ts",
            "cargo build; rm -rf x",
            "cargo build | tee log",
        ];
        for &line in must_not_match {
            assert!(
                !pattern_matches(line),
                "Pattern should NOT have matched '{line}'"
            );
        }

        // 3. Explicitly assert patterns DO match ordinary commands
        let must_match = &[
            "cargo test --workspace --quiet",
            "cargo tree --depth 1",
            r"C:\Users\x\.cargo\bin\cargo.exe test",
            "npm ci",
            "npx tsc --noEmit",
            "where cargo",
            "$env:PATHEXT",
            "node --check cockpit/ui/main.js",
            "deno check src/index.ts",
            "bun --check src/index.js",
        ];
        for &line in must_match {
            assert!(
                pattern_matches(line),
                "Pattern SHOULD have matched '{line}'"
            );
        }

        // 4. Assert is_safe_command_segment returns expected boolean
        assert!(!is_safe_command_segment("cargo install --git https://x/y"));
        assert!(!is_safe_command_segment("cargo publish"));
        assert!(!is_safe_command_segment("npm publish"));
        assert!(!is_safe_command_segment("npm install -g x"));
        assert!(!is_safe_command_segment("mvn deploy"));
        assert!(!is_safe_command_segment("node app.js"));
        assert!(!is_safe_command_segment("node -e console.log(1)"));
        assert!(!is_safe_command_segment("node check main.js"));
        assert!(!is_safe_command_segment("bun check main.js"));
        assert!(!is_safe_command_segment("node --check -e code"));
        assert!(!is_safe_command_segment("deno run -A app.ts"));

        assert!(is_safe_command_segment("cargo test --quiet"));
        assert!(is_safe_command_segment("npm install"));
        assert!(is_safe_command_segment("npx tsc"));
        assert!(is_safe_command_segment("node --check main.js"));
        assert!(is_safe_command_segment("deno check main.ts"));
        assert!(is_safe_command_segment("bun --check main.js"));
    }
}
