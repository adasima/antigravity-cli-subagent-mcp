//! `agy-mcp setup` — one place to see and change how permissions are handled.
//!
//! **Why this exists.** The posture is spread across four files owned by three
//! different programs: the MCP registration (`~/.claude.json` for Claude Code,
//! `%APPDATA%\Claude\claude_desktop_config.json` for Claude Desktop), agy's
//! `settings.json`, agy's shared `hooks.json`, and this bridge's pinned guard
//! copy. Nobody should have to hold that map in their head to answer "what is
//! actually on right now" — and getting it wrong is quiet: a guard installed
//! while the gate is still on is never consulted, and an `-e` added to the wrong
//! client does nothing at all (both observed while building this).
//!
//! So `show` reads the real files rather than assuming, `apply` is idempotent,
//! and `remove` puts the machine back.

use std::path::{Path, PathBuf};

use crate::hookguard;

/// How delegated runs are permitted to act.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Posture {
    /// As shipped: agy's gate, the pre-granted families, soft-deny recovery.
    Gate,
    /// `--dangerously-skip-permissions` on every delegated spawn. No check.
    Trusted,
    /// Gate off machine-wide, denylist guard back on for delegated runs only.
    Guarded,
}

impl Posture {
    pub fn name(self) -> &'static str {
        match self {
            Posture::Gate => "gate",
            Posture::Trusted => "trusted",
            Posture::Guarded => "guarded",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gate" | "default" => Some(Posture::Gate),
            "trusted" => Some(Posture::Trusted),
            "guarded" | "guard" => Some(Posture::Guarded),
            _ => None,
        }
    }
}

/// What is actually on disk. Read once, then reasoned about purely.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct State {
    /// Our entry is present in agy's shared `hooks.json`.
    pub hook_registered: bool,
    /// The pinned guard binary exists (fail-closed depends on it).
    pub guard_binary: bool,
    /// agy's persisted `toolPermission`; `None` = key absent = `request-review`.
    pub tool_permission: Option<String>,
    /// Client configs that carry `AGY_PERMISSIONS=trusted`, by path.
    pub trusted_clients: Vec<PathBuf>,
    /// Client configs that register `agy` at all, by path.
    pub known_clients: Vec<PathBuf>,
}

impl State {
    fn gate_is_off(&self) -> bool {
        self.tool_permission.as_deref() == Some("always-proceed")
    }
}

/// The active posture plus anything that is set but not doing what it looks like.
/// Pure, so every confusing combination we hit while building this stays covered.
pub fn assess(s: &State) -> (Posture, Vec<String>) {
    let mut warn = Vec::new();
    let posture = if !s.trusted_clients.is_empty() {
        Posture::Trusted
    } else if s.hook_registered && s.gate_is_off() {
        Posture::Guarded
    } else {
        Posture::Gate
    };

    if s.hook_registered && !s.gate_is_off() && posture != Posture::Trusted {
        warn.push(
            "the guard is installed but agy's gate is still on, so the guard is never \
             consulted (agy soft-denies first) - set toolPermission to always-proceed, or \
             remove the guard"
                .into(),
        );
    }
    if s.gate_is_off() && !s.hook_registered {
        warn.push(
            "agy's gate is off machine-wide and NO guard is installed - delegated runs can \
             do anything, including writing outside the workspace"
                .into(),
        );
    }
    if s.hook_registered && !s.guard_binary {
        warn.push(
            "the hook is registered but its binary is missing - agy fails CLOSED, so every \
             tool call on this machine is blocked. Run `agy-mcp install-hook` to restore it"
                .into(),
        );
    }
    if !s.trusted_clients.is_empty() && s.hook_registered {
        warn.push(
            "AGY_PERMISSIONS=trusted and the guard are both set; trusted skips agy's own \
             gate, and whether hooks still run under it has not been measured here"
                .into(),
        );
    }
    if s.known_clients.len() > 1 && !s.trusted_clients.is_empty() {
        warn.push(
            "more than one client registers agy; an -e only affects the one you actually \
             launch from"
                .into(),
        );
    }
    (posture, warn)
}

// ---------------------------------------------------------------------------
// Reading the machine
// ---------------------------------------------------------------------------

fn agy_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".gemini")
            .join("antigravity-cli")
            .join("settings.json")
    })
}

/// Every client config that might register this server. Claude Code and Claude
/// Desktop keep separate files, which is exactly the trap `show` exists to avoid.
fn client_config_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = dirs::home_dir() {
        out.push(h.join(".claude.json"));
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        out.push(
            PathBuf::from(appdata)
                .join("Claude")
                .join("claude_desktop_config.json"),
        );
    }
    out
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Find `mcpServers.agy` anywhere in a client config (Claude Code nests it under
/// per-project keys as well as at the root).
fn agy_entries(v: &serde_json::Value) -> Vec<&serde_json::Value> {
    let mut found = Vec::new();
    fn walk<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
        if let Some(obj) = v.as_object() {
            if let Some(servers) = obj.get("mcpServers").and_then(|s| s.as_object()) {
                if let Some(agy) = servers.get("agy") {
                    out.push(agy);
                }
            }
            for val in obj.values() {
                walk(val, out);
            }
        }
    }
    walk(v, &mut found);
    found
}

pub fn read_state() -> State {
    let mut s = State::default();

    if let Some(hooks) = dirs::home_dir()
        .map(|h| h.join(".gemini").join("config").join("hooks.json"))
        .as_deref()
        .and_then(read_json)
    {
        s.hook_registered = hooks.get(hookguard::HOOK_KEY).is_some();
    }
    s.guard_binary = hookguard::guard_exe_path()
        .map(|p| p.exists())
        .unwrap_or(false);

    if let Some(v) = agy_settings_path().as_deref().and_then(read_json) {
        s.tool_permission = v
            .get("toolPermission")
            .and_then(|t| t.as_str())
            .map(|t| t.to_string());
    }

    for path in client_config_paths() {
        let Some(v) = read_json(&path) else { continue };
        let entries = agy_entries(&v);
        if entries.is_empty() {
            continue;
        }
        s.known_clients.push(path.clone());
        let trusted = entries.iter().any(|e| {
            e.get("env")
                .and_then(|env| env.get("AGY_PERMISSIONS"))
                .and_then(|p| p.as_str())
                .map(|p| p.eq_ignore_ascii_case("trusted"))
                .unwrap_or(false)
        });
        if trusted {
            s.trusted_clients.push(path);
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Set or clear `toolPermission` in agy's settings. Pure; never touches another
/// key, and refuses a file it cannot parse rather than replacing it.
pub fn set_tool_permission(existing: &str, value: Option<&str>) -> anyhow::Result<Option<String>> {
    let mut root: serde_json::Value = if existing.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(existing).map_err(|e| {
            anyhow::anyhow!("agy's settings.json is not valid JSON ({e}) - refusing to write")
        })?
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("agy's settings.json is not a JSON object"))?;
    let current = obj.get("toolPermission").and_then(|v| v.as_str());
    match value {
        Some(v) => {
            if current == Some(v) {
                return Ok(None);
            }
            obj.insert("toolPermission".into(), serde_json::json!(v));
        }
        None => {
            if obj.remove("toolPermission").is_none() {
                return Ok(None);
            }
        }
    }
    Ok(Some(serde_json::to_string_pretty(&root)?))
}

fn write_atomic(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.agytmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn apply_tool_permission(value: Option<&str>) -> anyhow::Result<bool> {
    let Some(path) = agy_settings_path() else {
        anyhow::bail!("cannot locate the home directory");
    };
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    match set_tool_permission(&existing, value)? {
        None => Ok(false),
        Some(text) => {
            write_atomic(&path, &text)?;
            Ok(true)
        }
    }
}

/// Run `install-hook` logic in-process by shelling to ourselves. Reusing the
/// subcommand keeps ONE implementation of the pinned-copy dance.
fn run_self(args: &[&str]) -> anyhow::Result<String> {
    let exe = std::env::current_exe()?;
    let out = std::process::Command::new(exe).args(args).output()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(text)
}

fn describe(s: &State) -> String {
    let (posture, warn) = assess(s);
    let mut out = format!("posture: {}\n", posture.name());
    out.push_str(&format!(
        "  agy gate        {}\n",
        match s.tool_permission.as_deref() {
            Some("always-proceed") => "off (toolPermission = always-proceed)".to_string(),
            Some(other) => format!("on (toolPermission = {other})"),
            None => "on (toolPermission unset = request-review)".to_string(),
        }
    ));
    out.push_str(&format!(
        "  guard hook      {}\n",
        if s.hook_registered {
            if s.guard_binary {
                "registered, binary present"
            } else {
                "registered, BINARY MISSING"
            }
        } else {
            "not registered"
        }
    ));
    if s.known_clients.is_empty() {
        out.push_str("  clients         none found registering `agy`\n");
    }
    for c in &s.known_clients {
        let trusted = s.trusted_clients.contains(c);
        out.push_str(&format!(
            "  client          {} {}\n",
            c.display(),
            if trusted {
                "[AGY_PERMISSIONS=trusted]"
            } else {
                ""
            }
        ));
    }
    for w in warn {
        out.push_str(&format!("\n  ! {w}\n"));
    }
    out
}

const SETUP_USAGE: &str = "\
agy-mcp setup — inspect or change how delegated runs are permitted to act.

  agy-mcp setup show              What is actually on right now (reads the real files).
  agy-mcp setup apply <posture>   gate | trusted | guarded
  agy-mcp setup remove            Same as `apply gate`.

Postures:
  gate      As shipped. agy's own gate, pre-granted build/test families, soft-deny
            recovery. A command outside that can still end a run with no answer.
  guarded   agy's gate off machine-wide, and a PreToolUse guard put back for
            DELEGATED runs only. A refusal costs a sentence instead of the run;
            your own agy sessions are waved through. No client restart needed.
  trusted   --dangerously-skip-permissions on delegated spawns. Nothing dies, and
            nothing is checked. Lives on the MCP registration, so it needs a
            client restart to take effect.

See docs/permissions.md for what each one costs.";

pub(crate) fn run_setup(rest: &[String]) -> ! {
    let sub = rest.first().map(|s| s.as_str()).unwrap_or("show");
    match sub {
        "show" => {
            print!("{}", describe(&read_state()));
            std::process::exit(0)
        }
        "remove" => apply_posture(Posture::Gate),
        "apply" => match rest.get(1).and_then(|p| Posture::parse(p)) {
            Some(p) => apply_posture(p),
            None => {
                eprintln!(
                    "agy-mcp setup: name a posture — gate | trusted | guarded\n\n{SETUP_USAGE}"
                );
                std::process::exit(2)
            }
        },
        "-h" | "--help" | "help" => {
            println!("{SETUP_USAGE}");
            std::process::exit(0)
        }
        other => {
            eprintln!("agy-mcp setup: unknown command '{other}'\n\n{SETUP_USAGE}");
            std::process::exit(2)
        }
    }
}

fn apply_posture(target: Posture) -> ! {
    let mut steps: Vec<String> = Vec::new();
    let fail = |e: anyhow::Error| -> ! {
        eprintln!("agy-mcp setup: {e:#}");
        std::process::exit(2)
    };

    match target {
        Posture::Guarded => {
            match run_self(&["install-hook"]) {
                Ok(_) => steps.push("registered the guard hook (pinned copy refreshed)".into()),
                Err(e) => fail(e),
            }
            match apply_tool_permission(Some("always-proceed")) {
                Ok(true) => steps.push("turned agy's gate off (toolPermission)".into()),
                Ok(false) => steps.push("agy's gate was already off".into()),
                Err(e) => fail(e),
            }
        }
        Posture::Gate => {
            match run_self(&["install-hook", "--uninstall"]) {
                Ok(_) => steps.push("removed the guard hook and its pinned copy".into()),
                Err(e) => fail(e),
            }
            match apply_tool_permission(None) {
                Ok(true) => steps.push("restored agy's gate (toolPermission cleared)".into()),
                Ok(false) => steps.push("agy's gate was already on".into()),
                Err(e) => fail(e),
            }
        }
        Posture::Trusted => {
            // Deliberately NOT automated. This one lives on the MCP registration,
            // which belongs to the client, differs per client, and needs a restart
            // either way -- so the honest move is to hand over the exact line
            // rather than edit someone's client config behind their back.
            println!(
                "`trusted` is a setup-time decision on the MCP registration, so it is applied\n\
                 by hand and takes effect on your next client restart:\n\n\
                   claude mcp remove agy -s user\n\
                   claude mcp add agy -s user -e AGY_PERMISSIONS=trusted -- agy-mcp\n\n\
                 For Claude Desktop, add the same env to the `agy` entry in\n\
                 %APPDATA%\\Claude\\claude_desktop_config.json:\n\n\
                   \"agy\": {{ \"command\": \"agy-mcp\", \"env\": {{ \"AGY_PERMISSIONS\": \"trusted\" }} }}\n\n\
                 Nothing was changed. Run `agy-mcp setup show` afterwards to confirm it took."
            );
            std::process::exit(0)
        }
    }

    let state = read_state();
    let (now, _) = assess(&state);
    println!("applied `{}`:", target.name());
    for s in &steps {
        println!("  - {s}");
    }
    println!("\n{}", describe(&state));
    if now != target {
        println!(
            "note: the effective posture is `{}`, not `{}` - see the warnings above.",
            now.name(),
            target.name()
        );
    }
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guarded_state() -> State {
        State {
            hook_registered: true,
            guard_binary: true,
            tool_permission: Some("always-proceed".into()),
            trusted_clients: vec![],
            known_clients: vec![PathBuf::from("c.json")],
        }
    }

    #[test]
    fn the_three_postures_are_recognized_from_disk_state() {
        let (p, w) = assess(&guarded_state());
        assert_eq!(p, Posture::Guarded);
        assert!(
            w.is_empty(),
            "clean guarded state should warn about nothing: {w:?}"
        );

        let (p, w) = assess(&State::default());
        assert_eq!(p, Posture::Gate);
        assert!(w.is_empty());

        let trusted = State {
            trusted_clients: vec![PathBuf::from("c.json")],
            known_clients: vec![PathBuf::from("c.json")],
            ..Default::default()
        };
        assert_eq!(assess(&trusted).0, Posture::Trusted);
    }

    #[test]
    fn a_guard_installed_behind_a_live_gate_is_called_out() {
        // Hit for real while building this: with the gate on, agy soft-denies
        // before the hook is ever consulted, so the guard looks installed and does
        // nothing.
        let s = State {
            tool_permission: None,
            ..guarded_state()
        };
        let (p, w) = assess(&s);
        assert_eq!(p, Posture::Gate);
        assert!(w.iter().any(|m| m.contains("never")), "{w:?}");
    }

    #[test]
    fn a_gate_turned_off_with_no_guard_is_called_out() {
        let s = State {
            hook_registered: false,
            ..guarded_state()
        };
        let (_, w) = assess(&s);
        assert!(w.iter().any(|m| m.contains("anything")), "{w:?}");
    }

    #[test]
    fn a_registered_hook_with_no_binary_is_the_loudest_case() {
        // Fail-closed: this blocks every tool on the machine, including the user's
        // own sessions.
        let s = State {
            guard_binary: false,
            ..guarded_state()
        };
        let (_, w) = assess(&s);
        assert!(w.iter().any(|m| m.contains("CLOSED")), "{w:?}");
    }

    #[test]
    fn tool_permission_edits_are_idempotent_and_never_clobber() {
        let existing = r#"{"colorScheme":"tokyo night","permissions":{"allow":["x"]}}"#;
        let out = set_tool_permission(existing, Some("always-proceed"))
            .unwrap()
            .unwrap();
        assert!(out.contains("always-proceed"));
        assert!(out.contains("tokyo night"), "must not drop other settings");
        assert!(out.contains("allow"));
        // Idempotent.
        assert_eq!(
            set_tool_permission(&out, Some("always-proceed")).unwrap(),
            None
        );
        // Clearing removes only that key.
        let cleared = set_tool_permission(&out, None).unwrap().unwrap();
        assert!(!cleared.contains("toolPermission"));
        assert!(cleared.contains("tokyo night"));
        assert_eq!(set_tool_permission(&cleared, None).unwrap(), None);
        // Never replace a file we cannot parse.
        assert!(set_tool_permission("{not json", Some("x")).is_err());
    }

    #[test]
    fn an_agy_entry_is_found_however_the_client_nests_it() {
        // Claude Code keeps per-project sections as well as a root one.
        let v: serde_json::Value = serde_json::from_str(
            r#"{"projects":{"G:/p":{"mcpServers":{"agy":{"env":{"AGY_PERMISSIONS":"trusted"}}}}},
                "mcpServers":{"agy":{"env":{}}}}"#,
        )
        .unwrap();
        assert_eq!(agy_entries(&v).len(), 2);
    }

    #[test]
    fn posture_names_round_trip() {
        for p in [Posture::Gate, Posture::Trusted, Posture::Guarded] {
            assert_eq!(Posture::parse(p.name()), Some(p));
        }
        assert_eq!(Posture::parse("GUARD"), Some(Posture::Guarded));
        assert_eq!(Posture::parse("nope"), None);
    }
}
