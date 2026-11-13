//! Detection and verification of workspace-level agy customizations (skills and MCP servers).
//!
//! Why this exists: agy silently drops skills whose frontmatter fails its strict YAML parser
//! (e.g. unquoted colons in values), logging only a single line in its internal log.
//! Furthermore, workspace MCP servers in `<ws>/.agents/mcp_config.json` remain active even
//! when an MCP scope is passed to agy_sub, which only swaps `~/.gemini/config/mcp_config.json`.
//! This module makes those customizations visible to callers and surfaces dropped skills.

use std::path::{Path, PathBuf};

/// Check result for a single skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCheck {
    pub name: String,
    pub path: PathBuf,
    pub problem: Option<String>,
}

/// Pure check for the frontmatter shapes agy was measured to DROP a skill over.
///
/// agy 1.2.2, 2026-09-13: no `---` block, an unterminated one, and an unquoted value that
/// contains `: ` or ends in `:`. A missing `name` or `description` is deliberately not
/// flagged — agy loads that skill (named `SKILL`, or with an empty description), and this
/// result is prefixed to every `agy_sub` answer, so it must never be a guess.
/// Returns Some(reason) naming the line and key when a drop is certain.
pub fn frontmatter_problem(text: &str) -> Option<String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();

    if lines.is_empty() || lines[0].trim() != "---" {
        return Some("missing frontmatter: file must start with '---'".to_string());
    }

    let mut close_idx = None;
    for (idx, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            close_idx = Some(idx);
            break;
        }
    }

    let Some(close_idx) = close_idx else {
        return Some("unterminated frontmatter: missing closing '---'".to_string());
    };

    for (idx, line) in lines[1..close_idx].iter().enumerate() {
        let line_no = idx + 2; // 1-based line number (line 1 is the opening '---')

        // Top-level lines only: skip empty lines, comments, and indented lines
        if line.trim().is_empty()
            || line.starts_with(' ')
            || line.starts_with('\t')
            || line.starts_with('-')
            || line.starts_with('#')
        {
            continue;
        }

        let Some((key, val)) = line.split_once(':') else {
            continue;
        };

        let key = key.trim().trim_matches('"').trim_matches('\'');
        if key.is_empty() {
            continue;
        }

        let val = val.trim();

        // An unquoted plain scalar containing ': ' — or ending in ':' — makes agy's YAML
        // parser fail with "mapping values are not allowed", and agy drops the skill.
        // Both measured 2026-09-13 against agy 1.2.2; a ` #` inside the value is not a drop.
        let is_double_quoted = val.starts_with('"') && val.ends_with('"') && val.len() >= 2;
        let is_single_quoted = val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2;
        let is_block_scalar = val.starts_with('|') || val.starts_with('>');

        if !is_double_quoted && !is_single_quoted && !is_block_scalar {
            if val.contains(": ") {
                return Some(format!(
                    "line {line_no}: key '{key}' has unquoted value containing ': ' (quote the value)"
                ));
            }
            if val.ends_with(':') {
                return Some(format!(
                    "line {line_no}: key '{key}' has unquoted value ending in ':' (quote the value)"
                ));
            }
        }
    }

    None
}

/// Resolve a relative entry path against the directory containing `skills.json`, expanding `~`.
fn resolve_entry_path(entry_path: &str, base_dir: &Path) -> PathBuf {
    if entry_path == "~" {
        return dirs::home_dir().unwrap_or_default();
    }
    if let Some(rest) = entry_path
        .strip_prefix("~/")
        .or_else(|| entry_path.strip_prefix("~\\"))
    {
        return dirs::home_dir().unwrap_or_default().join(rest);
    }
    let p = Path::new(entry_path);
    if p.is_relative() {
        base_dir.join(p)
    } else {
        p.to_path_buf()
    }
}

/// Read a `skills.json` file if it exists and append resolved root paths.
fn read_skills_json(path: &Path, roots: &mut Vec<PathBuf>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    #[derive(serde::Deserialize)]
    struct SkillsFile {
        #[serde(default)]
        entries: Vec<SkillEntry>,
    }
    #[derive(serde::Deserialize)]
    struct SkillEntry {
        path: String,
    }
    let Ok(data) = serde_json::from_str::<SkillsFile>(&content) else {
        return;
    };
    let base = path.parent().unwrap_or(Path::new("."));
    for entry in data.entries {
        roots.push(resolve_entry_path(&entry.path, base));
    }
}

/// Discover all candidate skill root directories for a workspace and global configuration.
///
/// Order: workspace roots (`.agents/skills`, `_agents/skills`, `.agents/skills.json` entries),
/// followed by global roots (`~/.gemini/config/skills`, `~/.gemini/config/skills.json` entries).
pub fn skill_roots(ws: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Some(ws) = ws {
        roots.push(ws.join(".agents").join("skills"));
        roots.push(ws.join("_agents").join("skills"));
        read_skills_json(&ws.join(".agents").join("skills.json"), &mut roots);
    }

    let global_config = dirs::home_dir()
        .unwrap_or_default()
        .join(".gemini")
        .join("config");
    roots.push(global_config.join("skills"));
    read_skills_json(&global_config.join("skills.json"), &mut roots);

    let mut deduped = Vec::new();
    for r in roots {
        if !deduped.contains(&r) {
            deduped.push(r);
        }
    }
    deduped
}

/// Inspect all candidate skill roots for `*/SKILL.md` files and validate their frontmatter.
///
/// Returns checks sorted by skill directory name.
pub fn check_skills(roots: &[PathBuf]) -> Vec<SkillCheck> {
    let mut out = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();

    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skill_md = path.join("SKILL.md");
                if skill_md.is_file() {
                    // The same skill is routinely reachable twice: `~/.gemini/config/skills/zenith`
                    // is a link onto a directory that `skills.json` also lists. Compare the real
                    // file, or one broken skill is counted and warned about twice.
                    let real =
                        std::fs::canonicalize(&skill_md).unwrap_or_else(|_| skill_md.clone());
                    if !seen_paths.insert(real) {
                        continue;
                    }
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();
                    let problem = match std::fs::read_to_string(&skill_md) {
                        Ok(content) => frontmatter_problem(&content),
                        Err(e) => Some(format!("cannot read file: {e}")),
                    };
                    out.push(SkillCheck {
                        name,
                        path: skill_md,
                        problem,
                    });
                }
            }
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    out
}

/// Read MCP server names from `<ws>/.agents/mcp_config.json`.
///
/// Returns Ok(empty) if the file does not exist, or Err with the failure reason.
pub fn workspace_mcp_servers(ws: &Path) -> Result<Vec<String>, String> {
    let p = ws.join(".agents").join("mcp_config.json");
    if !p.exists() {
        return Ok(Vec::new());
    }
    let content = match std::fs::read_to_string(&p) {
        Ok(c) => c,
        Err(e) => return Err(format!("cannot read {}: {e}", p.display())),
    };
    let val: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => return Err(format!("invalid JSON in {}: {e}", p.display())),
    };
    let Some(servers) = val.get("mcpServers") else {
        return Ok(Vec::new());
    };
    let Some(obj) = servers.as_object() else {
        return Err(format!("`mcpServers` in {} is not an object", p.display()));
    };
    let mut names: Vec<String> = obj.keys().cloned().collect();
    names.sort();
    Ok(names)
}

/// Format customizations status for `agy_diag(status)`.
///
/// Pure. Kept short because it goes directly into model context.
pub fn customizations_status(
    skills: &[SkillCheck],
    ws_mcp: Option<&Result<Vec<String>, String>>,
) -> String {
    let loadable = skills.iter().filter(|s| s.problem.is_none()).count();
    let broken: Vec<&SkillCheck> = skills.iter().filter(|s| s.problem.is_some()).collect();

    let mut lines = Vec::new();
    if broken.is_empty() {
        lines.push(format!("skills: {loadable} loadable"));
    } else {
        lines.push(format!(
            "skills: {loadable} loadable, {} broken:",
            broken.len()
        ));
        for b in broken {
            let reason = b.problem.as_deref().unwrap_or("unknown error");
            lines.push(format!(
                "  ⚠ {} — {} ({})",
                b.name,
                reason,
                b.path.display()
            ));
        }
    }

    let mcp_str = match ws_mcp {
        Some(Ok(servers)) if !servers.is_empty() => servers.join(", "),
        Some(Ok(_)) | None => "none".to_string(),
        Some(Err(e)) => e.clone(),
    };
    lines.push(format!("workspace mcp: {mcp_str}"));

    lines.join("\n")
}

/// Warning paragraph when skills are dropped by agy's YAML parser.
///
/// Pure. Returns None when all skills are valid.
pub fn broken_skills_warning(skills: &[SkillCheck]) -> Option<String> {
    let broken: Vec<&SkillCheck> = skills.iter().filter(|s| s.problem.is_some()).collect();
    if broken.is_empty() {
        return None;
    }
    let mut lines = vec![format!(
        "⚠ agy will drop {} skill(s) due to frontmatter errors:",
        broken.len()
    )];
    for b in broken {
        let reason = b.problem.as_deref().unwrap_or("unknown error");
        lines.push(format!(
            "  · {} — {} ({})",
            b.name,
            reason,
            b.path.display()
        ));
    }
    Some(lines.join("\n"))
}

/// Advisory note when the run scopes MCP servers but workspace MCP servers remain active.
///
/// Pure.
pub fn mcp_scope_note(mode_is_inherit: bool, ws_servers: &[String]) -> Option<String> {
    if mode_is_inherit || ws_servers.is_empty() {
        return None;
    }
    Some(format!(
        "⚠ the `mcp` arg scopes ~/.gemini/config/mcp_config.json only; workspace server(s) remain active: {}",
        ws_servers.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_colon_ok() {
        let text = "---\nname: my-skill\ndescription: \"Spec for: Arknights Endfield\"\n---\nBody";
        assert_eq!(frontmatter_problem(text), None);
    }

    #[test]
    fn single_quoted_ok() {
        let text = "---\nname: my-skill\ndescription: 'Spec for: Arknights Endfield'\n---\nBody";
        assert_eq!(frontmatter_problem(text), None);
    }

    #[test]
    fn block_scalar_ok() {
        let text_pipe =
            "---\nname: my-skill\ndescription: |\n  Spec for: Arknights Endfield\n---\nBody";
        assert_eq!(frontmatter_problem(text_pipe), None);

        let text_fold =
            "---\nname: my-skill\ndescription: >\n  Spec for: Arknights Endfield\n---\nBody";
        assert_eq!(frontmatter_problem(text_fold), None);
    }

    #[test]
    fn unquoted_colon_flagged_with_right_line_number() {
        let text = "---\nname: my-skill\ndescription: Spec for the (Arknights: Endfield) avatar\n---\nBody";
        let prob = frontmatter_problem(text).expect("unquoted colon must be caught");
        assert!(prob.contains("line 3"), "expected line 3 in: {prob}");
        assert!(
            prob.contains("description"),
            "expected key 'description' in: {prob}"
        );
        assert!(
            prob.contains("quote"),
            "expected quote recommendation in: {prob}"
        );
    }

    #[test]
    fn unquoted_trailing_colon_flagged() {
        let text = "---
name: my-skill
description: Probe whose value ends with a colon:
---
Body";
        let prob = frontmatter_problem(text).expect("a trailing colon is dropped by agy too");
        assert!(
            prob.contains("line 3") && prob.contains("ending in ':'"),
            "{prob}"
        );

        let quoted = "---
name: my-skill
description: \"ends with a colon:\"
---
Body";
        assert_eq!(frontmatter_problem(quoted), None);
    }

    #[test]
    fn plain_ok() {
        let text = "---\nname: my-skill\ndescription: Plain text without colons\n---\nBody";
        assert_eq!(frontmatter_problem(text), None);

        let text_no_space_colon =
            "---\nname: my-skill\ndescription: http://example.com/api\n---\nBody";
        assert_eq!(frontmatter_problem(text_no_space_colon), None);
    }

    #[test]
    fn crlf() {
        let text_crlf = "---\r\nname: my-skill\r\ndescription: Plain CRLF text\r\n---\r\nBody";
        assert_eq!(frontmatter_problem(text_crlf), None);

        let text_crlf_broken = "---\r\nname: my-skill\r\ndescription: Spec: avatar\r\n---\r\nBody";
        let prob =
            frontmatter_problem(text_crlf_broken).expect("unquoted colon in CRLF must be caught");
        assert!(prob.contains("line 3"), "expected line 3 in: {prob}");
        assert!(
            prob.contains("description"),
            "expected key 'description' in: {prob}"
        );
    }

    #[test]
    fn no_frontmatter() {
        let text_empty = "";
        assert!(frontmatter_problem(text_empty).is_some());

        let text_no_dash = "# Heading\nsome content";
        assert!(frontmatter_problem(text_no_dash).is_some());

        let text_unterminated = "---\nname: my-skill\ndescription: test\n";
        let prob = frontmatter_problem(text_unterminated).expect("unterminated must be caught");
        assert!(
            prob.contains("unterminated"),
            "expected unterminated in: {prob}"
        );
    }

    #[test]
    fn missing_name_or_description_is_not_a_drop() {
        // Measured: agy loads both (the nameless one as `SKILL`). Flagging them would put a
        // false warning on every answer.
        assert_eq!(frontmatter_problem("---\nname: my-skill\n---\nBody"), None);
        assert_eq!(
            frontmatter_problem("---\ndescription: my description\n---\nBody"),
            None
        );
    }

    #[test]
    fn status_warning_note_formatting() {
        let valid_skill = SkillCheck {
            name: "good-skill".to_string(),
            path: PathBuf::from("/ws/skills/good-skill/SKILL.md"),
            problem: None,
        };
        let broken_skill = SkillCheck {
            name: "bad-skill".to_string(),
            path: PathBuf::from("/ws/skills/bad-skill/SKILL.md"),
            problem: Some(
                "line 3: key 'description' has unquoted value containing ': ' (quote the value)"
                    .to_string(),
            ),
        };

        // Status formatting: clean
        let clean_status = customizations_status(
            std::slice::from_ref(&valid_skill),
            Some(&Ok(vec!["server-a".to_string()])),
        );
        assert!(clean_status.contains("skills: 1 loadable"));
        assert!(clean_status.contains("workspace mcp: server-a"));
        assert!(!clean_status.contains("broken"));

        // Status formatting: broken skill + mcp error
        let err_res = Err("cannot read mcp config".to_string());
        let broken_status =
            customizations_status(&[valid_skill, broken_skill.clone()], Some(&err_res));
        assert!(broken_status.contains("skills: 1 loadable, 1 broken:"));
        assert!(broken_status.contains("bad-skill"));
        assert!(broken_status.contains("line 3"));
        assert!(broken_status.contains("workspace mcp: cannot read mcp config"));

        // Status formatting: None ws_mcp
        let none_status = customizations_status(&[], None);
        assert!(none_status.contains("skills: 0 loadable"));
        assert!(none_status.contains("workspace mcp: none"));

        // Broken skills warning
        assert_eq!(broken_skills_warning(&[]), None);
        let warn = broken_skills_warning(&[broken_skill]).expect("broken skill should warn");
        assert!(warn.starts_with('⚠'), "warning must start with ⚠");
        assert!(warn.contains("bad-skill"));
        assert!(warn.contains("line 3"));

        // MCP scope note
        let ws_servers = vec!["local-db".to_string()];
        // inherit: returns None
        assert_eq!(mcp_scope_note(true, &ws_servers), None);
        // empty ws servers: returns None
        assert_eq!(mcp_scope_note(false, &[]), None);
        // scoped + active ws servers: returns note
        let note =
            mcp_scope_note(false, &ws_servers).expect("scoped with ws servers should return note");
        assert!(note.starts_with('⚠'), "note must start with ⚠");
        assert!(note.contains("local-db"));
        assert!(note.contains("~/.gemini/config/mcp_config.json"));
    }

    #[test]
    fn skill_roots_resolves_relative_skills_json_entry() {
        let unique_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("agy_test_skills_{unique_id}"));
        let agents_dir = temp_dir.join(".agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        let json_content = r#"{"entries":[{"path":"custom_skills"},{"path":"~/global_test"}]}"#;
        std::fs::write(agents_dir.join("skills.json"), json_content).unwrap();

        let roots = skill_roots(Some(&temp_dir));

        let expected_rel = agents_dir.join("custom_skills");
        assert!(
            roots.contains(&expected_rel),
            "expected roots to contain resolved relative path: {expected_rel:?}, got: {roots:?}"
        );

        let home = dirs::home_dir().unwrap_or_default();
        let expected_home = home.join("global_test");
        assert!(
            roots.contains(&expected_home),
            "expected roots to contain resolved ~ path: {expected_home:?}, got: {roots:?}"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
