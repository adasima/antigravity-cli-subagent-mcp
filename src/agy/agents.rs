//! Custom agents a preset runs under (`--agent <name>`).
//!
//! An agent's `tools:` list is enforced by agy, not by the prompt: a run under a
//! read-only agent has no edit or shell tool to call (measured, design-notes "Custom
//! agents … as a preset base"). Definitions agy-mcp ships live in the binary and are
//! written to the user-level dir, which agy finds from any workspace (measured
//! 2026-10-29, agy 1.4.9). A workspace never has to carry them.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Definitions shipped in the binary: (agent name, file contents).
const BUILTIN_AGENTS: &[(&str, &str)] = &[(
    "agy-mcp-research",
    include_str!("../presets/agents/agy-mcp-research.md"),
)];

/// agy's user-level agent dir, `~/.gemini/config/agents`.
pub fn user_agents_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".gemini")
        .join("config")
        .join("agents")
}

/// The shipped contents for `name`, or None for an agent agy-mcp does not ship
/// (a user's own; agy itself refuses an unknown name with exit 1). Pure.
pub(crate) fn builtin_agent(name: &str) -> Option<&'static str> {
    BUILTIN_AGENTS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, content)| *content)
}

/// Make `<dir>/<name>.md` hold the shipped definition when `name` is one agy-mcp
/// ships; a no-op for any other name. Rewrites only when the file is missing or
/// differs, via temp + rename (concurrent runs may race here; a torn read would
/// make agy drop the agent and fail the run). Split from [`ensure_builtin_agent`]
/// so tests can point it at a temp dir.
pub(crate) fn ensure_builtin_agent_in(dir: &Path, name: &str) -> Result<()> {
    let Some(shipped) = builtin_agent(name) else {
        return Ok(());
    };
    let target = dir.join(format!("{name}.md"));
    if let Ok(existing) = std::fs::read_to_string(&target) {
        if existing == shipped {
            return Ok(());
        }
    }
    std::fs::create_dir_all(dir)?;
    let pid = std::process::id();
    let tmp = dir.join(format!("{name}.md.tmp-{pid}"));
    std::fs::write(&tmp, shipped)?;
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// [`ensure_builtin_agent_in`] on [`user_agents_dir`].
pub(crate) fn ensure_builtin_agent(name: &str) -> Result<()> {
    ensure_builtin_agent_in(&user_agents_dir(), name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_agent_known_and_unknown() {
        let known = builtin_agent("agy-mcp-research");
        assert!(known.is_some());
        assert!(known.unwrap().contains("agy-mcp-research"));
        assert_eq!(builtin_agent("unknown-agent"), None);
    }

    #[test]
    fn ensure_builtin_agent_writes_missing_file() {
        let unique_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("agy_test_agents_missing_{unique_id}"));
        let target = temp_dir.join("agy-mcp-research.md");

        ensure_builtin_agent_in(&temp_dir, "agy-mcp-research").unwrap();
        assert!(target.is_file());
        let content = std::fs::read_to_string(&target).unwrap();
        assert_eq!(content, builtin_agent("agy-mcp-research").unwrap());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn ensure_builtin_agent_overwrites_differing_file() {
        let unique_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("agy_test_agents_differ_{unique_id}"));
        let target = temp_dir.join("agy-mcp-research.md");
        std::fs::create_dir_all(&temp_dir).unwrap();
        std::fs::write(&target, "old differing content").unwrap();

        ensure_builtin_agent_in(&temp_dir, "agy-mcp-research").unwrap();
        let content = std::fs::read_to_string(&target).unwrap();
        assert_eq!(content, builtin_agent("agy-mcp-research").unwrap());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn ensure_builtin_agent_leaves_identical_file_alone() {
        let unique_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("agy_test_agents_identical_{unique_id}"));
        let target = temp_dir.join("agy-mcp-research.md");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let shipped = builtin_agent("agy-mcp-research").unwrap();
        std::fs::write(&target, shipped).unwrap();

        let meta_before = std::fs::metadata(&target).unwrap();
        let mtime_before = meta_before.modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        ensure_builtin_agent_in(&temp_dir, "agy-mcp-research").unwrap();

        let meta_after = std::fs::metadata(&target).unwrap();
        assert_eq!(mtime_before, meta_after.modified().unwrap());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn ensure_builtin_agent_unknown_name_creates_nothing() {
        let unique_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("agy_test_agents_unknown_{unique_id}"));

        ensure_builtin_agent_in(&temp_dir, "nonexistent-agent").unwrap();
        assert!(!temp_dir.exists());
    }
}
