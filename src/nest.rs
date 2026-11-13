//! agy-mcp inside an agy that agy-mcp started — not supported (D17, retiring D11).
//!
//! agy starts every server in its shared `mcp_config.json` for every agy process, including the
//! ones agy-mcp spawns (runs, `agy models`, `/usage`, `--version`). When agy-mcp is registered
//! there (the Antigravity GUI uses that file), each of those agy processes starts another
//! agy-mcp. Unmarked, that server took itself for top-level, ran `agy models` at startup, and
//! its agy started the next one: a process loop (2026-10-29). Delegating through it (Claude →
//! agy-mcp → agy → agy-mcp → agy) is also not something we support: same-model sub-agents are
//! agy's own.
//!
//! So every agy agy-mcp spawns carries [`DEPTH_ENV`], and an agy-mcp that finds it exits before
//! the handshake. agy retries such a server a few times, then runs without it, silently
//! (measured 2026-10-29: 3 starts, exit 0, correct answer). Starting it cannot be prevented from
//! here — the config is shared — so this is the stop.

use std::ffi::OsStr;

/// Set on every agy agy-mcp spawns. Any value marks "inside an agy that agy-mcp started";
/// binaries before D17 set it to a depth number, which reads the same.
pub const DEPTH_ENV: &str = "AGY_MCP_DEPTH";

/// The env for an agy agy-mcp spawns, whatever it is for.
pub fn spawned_env() -> [(&'static str, &'static str); 1] {
    [(DEPTH_ENV, "1")]
}

/// Pure core of [`inside_spawned_agy`].
fn marked(value: Option<&OsStr>) -> bool {
    value.is_some()
}

/// Whether this process is an agy-mcp that an agy-mcp-spawned agy started.
pub fn inside_spawned_agy() -> bool {
    marked(std::env::var_os(DEPTH_ENV).as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_spawned_agy_hands_down_marks_the_server_below() {
        let env = spawned_env();
        let value = env
            .iter()
            .find(|(k, _)| *k == DEPTH_ENV)
            .map(|(_, v)| OsStr::new(*v));
        assert!(marked(value));
        // Older binaries handed down a depth number; any value is a mark.
        assert!(marked(Some(OsStr::new("2"))));
        assert!(!marked(None));
    }
}
