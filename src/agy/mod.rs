//! Core logic for bridging the Antigravity CLI (`agy`).
//!
//! Why this exists: `agy -p "<prompt>"` (print / non-interactive mode) has one
//! hard pitfall plus a since-fixed history the layered read still guards against:
//!   1. It FREEZES at startup if stdin is left open (Antigravity CLI pitfall).
//!      => we always spawn it with stdin closed (`Stdio::null()`).
//!   2. Historically it printed NOTHING to stdout (upstream issue #76; the answer
//!      only reached agy's transcript). Fixed in agy v1.1.1 — `-p` now prints the
//!      answer to stdout — but the layered read below survives either behavior.
//!
//! Response is resolved with a layered fallback so the bridge keeps working
//! across agy's bug lifecycle (see `resolve_outcome`):
//!   (1) stdout — the answer today (v1.1.1+), after stripping agy's system blocks
//!       (`SYSTEM_BLOCK_TAGS`). Used only when the run is clean: no timeout, no
//!       spawn error, non-empty.
//!   (2) transcript.jsonl `PLANNER_RESPONSE` — fallback when stdout is empty or
//!       dirty (older agy, a regression, or a run that emitted no stdout).
//!   (3) SQLite `<id>.db` — agy 1.0.4+ dual-writes this; a best-effort byte
//!       scan is attempted if JSONL ever disappears (the payload is protobuf,
//!       so this is a last resort, not a full decoder).
//!
//! Model selection: catalog slugs internally (e.g. "gemini-3.1-pro-low" /
//! "claude-opus-4-6-thinking"); converted to display label on the wire via
//! `wire_model` for agy `--model`. The transcript echoes the served model in a
//! "Model Selection" from→to line, which we extract as a receipt (`session_model`).

pub mod agents;
pub mod customizations;
pub mod git;
pub mod jobobj;
pub mod jobs;
pub mod mcp;
pub mod models;
pub mod paths;
pub mod permissions;
pub mod quota;
pub mod run;
pub mod status;
pub mod transcript;

pub(crate) use customizations::*;
pub(crate) use git::*;
pub(crate) use jobs::*;
pub(crate) use mcp::*;
pub(crate) use models::*;
pub(crate) use paths::*;
pub(crate) use permissions::*;
pub(crate) use quota::*;
pub(crate) use run::*;
pub(crate) use status::*;
pub(crate) use transcript::*;
