use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;
/// agy's shared MCP config (read by the CLI, IDE and standalone alike).
pub fn mcp_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".gemini")
        .join("config")
        .join("mcp_config.json")
}

fn mcp_swap_bak() -> PathBuf {
    mcp_config_path().with_file_name("mcp_config.json.agyswapbak")
}

/// Which MCP servers the sub-agent may use for one run.
#[derive(Clone)]
pub enum McpMode {
    /// Leave agy's configured set as-is (no swap). Default.
    Inherit,
    /// No MCP servers (clean/fast/safe sub-agent).
    None,
    /// Only these servers (by name, case-insensitive).
    Only(Vec<String>),
}

/// Restores the original MCP config when dropped — so a swap is always undone,
/// even if the run errors, times out, or the future is cancelled.
///
/// agy reads this config **once, at startup** (measured 2026-08-14, see
/// design-notes), so the caller normally calls [`McpGuard::restore`] the moment
/// agy is up rather than waiting for the run to end. Drop stays as the backstop
/// for every path that never reaches that point, which is why `restore` is
/// idempotent instead of consuming self: both can fire, and the second must be a
/// no-op rather than re-writing a config someone else may since have swapped.
pub(crate) struct McpGuard {
    path: PathBuf,
    original: String,
    bak: PathBuf,
    restored: bool,
}
impl McpGuard {
    /// Put the user's config back now. Safe to call more than once.
    pub(crate) fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let _ = std::fs::write(&self.path, &self.original);
        let _ = std::fs::remove_file(&self.bak);
        let _ = std::fs::remove_file(mcp_swap_owner());
    }
}
impl Drop for McpGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Swap agy's MCP config for the duration of the returned guard. Returns None
/// for Inherit, or if the config can't be read/parsed (then we run as-is rather
/// than risk corrupting a file we don't understand). Writes a `.agyswapbak`
/// breadcrumb so a crash mid-run can be recovered on next startup.
pub(crate) async fn apply_mcp_swap(mode: &McpMode) -> Result<Option<McpGuard>> {
    if matches!(mode, McpMode::Inherit) {
        return Ok(None);
    }
    let path = mcp_config_path();
    let bak = mcp_swap_bak();
    // The breadcrumb IS the cross-process lock, claimed with `create_new` so the
    // check and the claim are one atomic step. A breadcrumb held by someone else
    // means another scoped run is mid-swap; reading `path` now would capture the
    // *already-swapped* config as our "original" and permanently lose the user's
    // real config when both guards drop. agy-mcp servers are per-session and
    // coexist, so we wait for the holder and then give up LOUDLY — silently
    // running with a wider MCP scope than the caller asked for would defeat the
    // point of scoping it. (Startup `recover_mcp_swap` clears a crash leftover.)
    let mut waited = 0u64;
    loop {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&bak)
            .await
        {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if waited >= MCP_SWAP_WAIT_S {
                    anyhow::bail!(
                        "another MCP-scoped agy run has held the shared mcp_config.json swap for \
                         >{MCP_SWAP_WAIT_S}s (possibly another Claude Code session). Retry, or run \
                         this one with the inherited MCP scope."
                    );
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                waited += 2;
            }
            Err(e) => return Err(anyhow::Error::new(e).context("claiming the MCP swap breadcrumb")),
        }
    }
    // From here the breadcrumb is ours: every early return must release it.
    //
    // Claim it in the OWNER's name before touching the config, not after. The sidecar
    // is what tells a concurrently-starting server "this breadcrumb belongs to a live
    // run"; while it is missing, `recover_mcp_swap` reads the breadcrumb as an
    // ownerless crash leftover and deletes it out from under us. Writing it after the
    // swap left exactly that window open, and the worst landing is inside it: with
    // `bak` still the empty file `create_new` made, the recovery skips the restore,
    // removes our breadcrumb, and a third process is then free to claim one and
    // capture the *swapped* config as its "original" — permanently losing the user's
    // real one, which is the accident the breadcrumb exists to prevent.
    let _ = tokio::fs::write(mcp_swap_owner(), std::process::id().to_string()).await;
    let original_res = tokio::fs::read_to_string(&path).await;

    let build = |mode: &McpMode, original: &str| -> Option<(String, String)> {
        let mut val: serde_json::Value = serde_json::from_str(original).ok()?;
        let servers = val.get_mut("mcpServers")?.as_object_mut()?;
        match mode {
            McpMode::None => servers.clear(),
            McpMode::Only(names) => {
                servers.retain(|k, _| names.iter().any(|n| n.eq_ignore_ascii_case(k)));
            }
            McpMode::Inherit => unreachable!(),
        }
        let swapped = serde_json::to_string_pretty(&val).ok()?;
        Some((original.to_string(), swapped))
    };

    // Never rewrite a config we couldn't read or parse — run as-is instead.
    let Some(original) = original_res.ok() else {
        let _ = tokio::fs::remove_file(&bak).await;
        let _ = tokio::fs::remove_file(mcp_swap_owner()).await;
        return Ok(None);
    };

    let Some((original, swapped)) = build(mode, &original) else {
        let _ = tokio::fs::remove_file(&bak).await;
        let _ = tokio::fs::remove_file(mcp_swap_owner()).await;
        return Ok(None);
    };

    if tokio::fs::write(&bak, &original).await.is_err()
        || tokio::fs::write(&path, swapped).await.is_err()
    {
        let _ = tokio::fs::remove_file(&bak).await;
        let _ = tokio::fs::remove_file(mcp_swap_owner()).await;
        return Ok(None);
    }
    Ok(Some(McpGuard {
        path,
        original,
        bak,
        restored: false,
    }))
}

/// How long a scoped run waits for another process's MCP swap to finish.
const MCP_SWAP_WAIT_S: u64 = 30;

fn mcp_swap_owner() -> PathBuf {
    mcp_config_path().with_file_name("mcp_config.json.agyswapbak.pid")
}

/// On startup, restore a swap left behind by a *crashed* run. With several
/// servers coexisting, a breadcrumb may belong to a live sibling's in-flight run
/// — restoring that one would yank the config out from under it, so we only
/// recover when the owner is gone (or unknown, i.e. a legacy breadcrumb).
pub fn recover_mcp_swap(is_alive: &dyn Fn(u32) -> bool) {
    let owner = std::fs::read_to_string(mcp_swap_owner())
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    if owner.is_some_and(is_alive) {
        return;
    }
    let bak = mcp_swap_bak();
    if let Ok(orig) = std::fs::read_to_string(&bak) {
        if !orig.is_empty() {
            let _ = std::fs::write(mcp_config_path(), orig);
        }
        let _ = std::fs::remove_file(&bak);
    }
    let _ = std::fs::remove_file(mcp_swap_owner());
}

/// List the MCP servers agy itself is configured with (so the caller knows what
/// tools the Gemini sub-agent can use). Secrets in args/env are redacted.
pub async fn mcp_servers() -> String {
    let path = mcp_config_path();
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(r) => r,
        Err(e) => return format!("agy MCP config not readable at {}: {e}", path.display()),
    };
    let val: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return format!(
                "agy MCP config at {} is not valid JSON: {e}",
                path.display()
            )
        }
    };
    let Some(servers) = val.get("mcpServers").and_then(|x| x.as_object()) else {
        return format!("no `mcpServers` object in {}", path.display());
    };
    let mut lines = vec![format!(
        "agy's configured MCP servers ({}):",
        path.display()
    )];
    let mut names: Vec<&String> = servers.keys().collect();
    names.sort();
    for name in names {
        let s = &servers[name];
        let disabled = s.get("disabled").and_then(|x| x.as_bool()).unwrap_or(false);
        let cmd = s.get("command").and_then(|x| x.as_str()).unwrap_or("?");
        let args = s
            .get("args")
            .and_then(|x| x.as_array())
            .map(|a| {
                let raw: Vec<&str> = a.iter().filter_map(|v| v.as_str()).collect();
                redact_args(&raw).join(" ")
            })
            .unwrap_or_default();
        let state = if disabled { "disabled" } else { "enabled " };
        lines.push(format!("  [{}] {:<18} {} {}", state, name, cmd, args));
    }
    lines.push(
        "(args/env redacted; some servers carry API keys. Edit the file above to change.)".into(),
    );
    lines.join("\n")
}

/// Redact an argv the way a secret is actually laid out in one: `--api-key VALUE`.
/// Pure.
///
/// **The per-element version had it exactly backwards.** `--api-key` contains "key",
/// so *the flag* was replaced with `***` while the secret sitting in the next element
/// went out verbatim unless it happened to be ≥24 chars — `["--api-key", "abc123…"]`
/// printed as `*** abc123…`. The flag name is not the secret; the thing after it is.
/// This output goes into a model's context via `agy_diag(mcp)`, so the direction of
/// that mistake matters.
///
/// So a secret-naming flag now (a) stays readable, because knowing *which* server
/// takes a key is the useful part, and (b) censors what follows it — the separate
/// element, or the value half of an inline `--api-key=VALUE`. Self-evidently secret
/// elements are still censored wherever they appear.
fn redact_args(args: &[&str]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut censor_next = false;
    for a in args {
        let take = std::mem::take(&mut censor_next);
        if take || looks_like_secret(a) {
            out.push("***".to_string());
            continue;
        }
        match a.split_once('=') {
            // `--api-key=VALUE`: keep the name, censor the value.
            Some((name, val)) if names_a_secret(name) && !val.is_empty() => {
                out.push(format!("{name}=***"));
            }
            // A bare `--api-key`: readable, but whatever follows is the secret.
            _ => {
                censor_next = names_a_secret(a);
                out.push(a.to_string());
            }
        }
    }
    out
}

/// Whether this element is a flag/key whose *value* is a secret.
fn names_a_secret(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    ["key", "secret", "token", "pass", "pwd", "auth", "cred"]
        .iter()
        .any(|p| lower.contains(p))
}

/// Whether this element is a secret on its own evidence, wherever it appears.
fn looks_like_secret(s: &str) -> bool {
    // A long opaque blob. Kept deliberately loose: over-censoring a config dump
    // costs nothing, printing one key costs everything.
    let opaque = s.len() >= 24
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));

    let lower = s.to_ascii_lowercase();
    let has_key_prefix = ["sk-", "ctx7sk", "aq."]
        .iter()
        .any(|p| lower.starts_with(p) || lower.contains(&format!("={}", p)));

    // `https://user:pw@host`
    let has_url_credentials = lower.contains("://") && lower.contains('@');

    opaque || has_key_prefix || has_url_credentials
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_flags_value_is_censored_and_the_flag_is_not() {
        // The regression this exists for: the flag was starred and the value printed.
        assert_eq!(
            redact_args(&["--api-key", "abc123def456ghi789"]),
            vec!["--api-key", "***"]
        );
        assert_eq!(
            redact_args(&["--token", "s3cr3t-value-1234"]),
            vec!["--token", "***"]
        );
        // Inline form: the name survives, the value does not.
        assert_eq!(redact_args(&["--api-key=abc123"]), vec!["--api-key=***"]);
        // A short, unremarkable value is exactly the case the old code leaked.
        assert_eq!(
            redact_args(&["--password", "hunter2"]),
            vec!["--password", "***"]
        );
    }

    #[test]
    fn self_evident_secrets_are_censored_anywhere() {
        assert_eq!(
            redact_args(&["sk-proj-AAAAAAAAAAAAAAAAAAAAAAAA"]),
            vec!["***"]
        );
        assert_eq!(
            redact_args(&["https://user:pw@example.com/mcp"]),
            vec!["***"]
        );
        assert_eq!(redact_args(&["AAAAAAAAAAAAAAAAAAAAAAAAAAAA"]), vec!["***"]);
    }

    #[test]
    fn ordinary_arguments_stay_readable() {
        // Censoring everything would make the listing useless, which is the point
        // of showing it at all — the caller needs to know what the sub-agent can reach.
        assert_eq!(
            redact_args(&["-y", "@modelcontextprotocol/server-filesystem", "/srv/data"]),
            vec!["-y", "@modelcontextprotocol/server-filesystem", "/srv/data"]
        );
        assert_eq!(redact_args(&["--port", "8080"]), vec!["--port", "8080"]);
        // Only the element right after the flag is censored, not the rest of the line.
        assert_eq!(
            redact_args(&["--api-key", "abc123", "--port", "8080"]),
            vec!["--api-key", "***", "--port", "8080"]
        );
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn bench_mcp_servers() {
        let start = Instant::now();
        for _ in 0..1000 {
            let _ = mcp_servers().await;
        }
        let duration = start.elapsed();
        println!("Time for 1000 runs: {:?}", duration);
    }
}
