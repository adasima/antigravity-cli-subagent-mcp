use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Resolve the `agy` executable: `AGY_BIN` env wins, else the known Windows
/// install path, else bare `agy` (relies on PATH).
pub fn agy_bin() -> String {
    if let Ok(b) = std::env::var("AGY_BIN") {
        if !b.is_empty() && Path::new(&b).is_absolute() {
            return b;
        }
    }
    #[cfg(windows)]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let p = PathBuf::from(local).join("agy").join("bin").join("agy.exe");
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    "agy".to_string()
}

pub fn data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".gemini")
        .join("antigravity-cli")
}
pub fn brain_dir() -> PathBuf {
    data_dir().join("brain")
}
pub fn conversations_dir() -> PathBuf {
    data_dir().join("conversations")
}

/// Resolve a (possibly abbreviated) conversation id to its brain directory.
/// Accepts a full id or any prefix — agy_sessions shows the short leading
/// segment, so callers can pass that directly. If several match, the newest
/// wins. Returns None if nothing matches.
pub fn resolve_conv_dir(id: &str) -> Option<PathBuf> {
    let exact = brain_dir().join(id);
    if exact.is_dir() {
        return Some(exact);
    }
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(brain_dir()).ok()?.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if e.file_name().to_string_lossy().starts_with(id) {
            let mt = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            match &best {
                Some((bm, _)) if *bm >= mt => {}
                _ => best = Some((mt, p)),
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Where agy runs when the caller doesn't pass a workspace.
///
/// IMPORTANT (cwd-less design): we never fall back to the process's current
/// directory — an MCP server launched by a desktop host inherits an
/// unpredictable cwd (often the app install dir or `/`). We use a stable,
/// dedicated directory under the user's home instead, created on demand.
pub fn default_workspace() -> PathBuf {
    // AGY_WORKSPACE (set at registration to the project root) wins, so an omitted
    // `workspace` runs agy IN the real project — reusing its `target/` build cache
    // for fast checks instead of a cold scratch dir. This is NOT the process cwd
    // (invariant #5): an explicitly configured path, predictable for a desktop-
    // launched server. Isolation is opt-in: pass an explicit `workspace` instead.
    if let Some(w) = std::env::var_os("AGY_WORKSPACE") {
        let p = PathBuf::from(w);
        if !p.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(&p);
            return p;
        }
    }
    let ws = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agy-mcp")
        .join("workspace");
    let _ = std::fs::create_dir_all(&ws);
    ws
}

/// Newest conversation directory under brain/ based on modification time.
pub fn newest_conv_dir() -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(brain_dir()).ok()?.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        match &best {
            Some((bm, _)) if *bm >= modified => {}
            _ => best = Some((modified, p)),
        }
    }
    best.map(|(_, p)| p)
}

/// Snapshot the set of conversation-dir names under brain/ (for fresh-run
/// attribution under concurrency: diff this against the post-spawn set).
pub(crate) fn brain_dir_names() -> std::collections::HashSet<String> {
    let mut s = std::collections::HashSet::new();
    if let Ok(rd) = std::fs::read_dir(brain_dir()) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                s.insert(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    s
}

/// Newest brain/ dir whose name is NOT in `before` — i.e. the conversation our
/// just-spawned run created. Reliable under concurrency because the caller holds
/// the spawn gate across snapshot→spawn→this call.
pub(crate) fn newest_dir_not_in(before: &std::collections::HashSet<String>) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(brain_dir()).ok()?.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if before.contains(&*e.file_name().to_string_lossy()) {
            continue;
        }
        let mt = e
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        match &best {
            Some((bm, _)) if *bm >= mt => {}
            _ => best = Some((mt, p)),
        }
    }
    best.map(|(_, p)| p)
}

pub(crate) fn transcript_path(conv_dir: &Path) -> PathBuf {
    conv_dir
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl")
}

/// Where running server instances announce themselves (one file per pid).
/// Informational only — instances coexist; this exists so `agy-mcp doctor` can
/// answer "is any server actually up?" without guessing.
pub fn instances_dir() -> PathBuf {
    agy_mcp_home().join("instances")
}

pub fn agy_mcp_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agy-mcp")
}

/// Announce this process as a live MCP server. Also reaps the markers of dead
/// instances, so the directory can't grow without bound (a crashed server never
/// gets to clean up after itself).
pub fn register_server_pid(is_alive: &dyn Fn(u32) -> bool) -> std::io::Result<()> {
    let dir = instances_dir();
    std::fs::create_dir_all(&dir)?;
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        if let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() {
            if !is_alive(pid) {
                let p = e.path();
                if std::fs::symlink_metadata(&p).is_ok_and(|m| !m.is_symlink()) {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
    }
    // Legacy single-instance lock: nothing reads it anymore, and leaving it
    // around only invites "why is there a lock?" confusion.
    let _ = std::fs::remove_file(agy_mcp_home().join("server.lock"));
    std::fs::write(
        dir.join(std::process::id().to_string()),
        now_unix().to_string(),
    )
}

/// PIDs of agy-mcp servers that are actually running (stale markers filtered).
/// Also honours the legacy single-instance lock, so a server from a build that
/// predates this registry is still reported instead of diagnosed as "none".
pub fn live_server_pids(is_alive: &dyn Fn(u32) -> bool) -> Vec<u32> {
    let legacy = std::fs::read_to_string(agy_mcp_home().join("server.lock"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    let mut pids: Vec<u32> = std::fs::read_dir(instances_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok())
        .chain(legacy)
        .filter(|p| is_alive(*p))
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

use crate::agy::quota::now_unix;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agy_bin_respects_absolute_path_only() {
        // Warning: This test modifies environment variables, which can cause
        // flakiness if tests are run in parallel.
        let old_agy_bin = std::env::var_os("AGY_BIN");

        let abs_path = if cfg!(windows) {
            r"C:\Windows\System32\cmd.exe"
        } else {
            "/usr/bin/echo"
        };
        std::env::set_var("AGY_BIN", abs_path);
        assert_eq!(agy_bin(), abs_path);

        let rel_path = "echo";
        std::env::set_var("AGY_BIN", rel_path);
        assert_ne!(agy_bin(), rel_path);

        let rel_path2 = "./echo";
        std::env::set_var("AGY_BIN", rel_path2);
        assert_ne!(agy_bin(), rel_path2);

        if let Some(old) = old_agy_bin {
            std::env::set_var("AGY_BIN", old);
        } else {
            std::env::remove_var("AGY_BIN");
        }
    }
}
