use std::process::Stdio;
use tokio::process::Command;

use crate::agy::jobs::{list_job_records, JobRecord};
use crate::agy::paths::{agy_bin, brain_dir, data_dir, default_workspace, newest_conv_dir};
use crate::agy::transcript::read_transcript;

/// An empty/unreadable transcript is not on its own evidence of a problem: a
/// conversation the permission gate refused (e.g. `cargo publish --dry-run`)
/// never gets an agy answer either, and that is the gate working as designed,
/// not a trust/OAuth/cross-drive fault. Pure so it's testable without the disk.
fn conv_was_denied(conv_id: &str, jobs: &[JobRecord]) -> bool {
    jobs.iter().any(|j| {
        j.conversation_id.as_deref() == Some(conv_id)
            && j.status == "failed"
            && j.result
                .as_deref()
                .is_some_and(|r| r.contains("soft-denied in headless mode"))
    })
}

/// Offline diagnostics. Spends no quota (only `agy --version` + filesystem reads).
pub async fn status() -> String {
    let mut rows: Vec<(String, bool, String)> = Vec::new();
    let bin = agy_bin();

    match Command::new(&bin)
        .arg("--version")
        .stdin(Stdio::null())
        .envs(crate::nest::spawned_env())
        .output()
        .await
    {
        Ok(o) if o.status.success() => rows.push((
            "agy CLI".into(),
            true,
            format!("{} (v{})", bin, String::from_utf8_lossy(&o.stdout).trim()),
        )),
        Ok(o) => rows.push((
            "agy CLI".into(),
            false,
            format!("{} ran but exited {:?}", bin, o.status.code()),
        )),
        Err(e) => rows.push((
            "agy CLI".into(),
            false,
            format!("{bin} not runnable: {e} (set AGY_BIN to full path)"),
        )),
    }

    let dd = data_dir();
    rows.push(("data dir".into(), dd.exists(), dd.display().to_string()));
    let bd = brain_dir();
    rows.push(("brain dir".into(), bd.exists(), bd.display().to_string()));
    let ws = default_workspace();
    rows.push((
        "default workspace".into(),
        ws.exists(),
        ws.display().to_string(),
    ));

    match newest_conv_dir() {
        None => rows.push((
            "newest transcript".into(),
            true,
            "no conversations yet".into(),
        )),
        Some(c) => match read_transcript(&c) {
            Ok((_, served)) => rows.push((
                "newest transcript".into(),
                true,
                format!(
                    "readable (last model: {})",
                    served.as_deref().unwrap_or("?")
                ),
            )),
            Err(e) => {
                let conv_id = c.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if conv_was_denied(conv_id, &list_job_records()) {
                    rows.push((
                        "newest transcript".into(),
                        true,
                        "empty — last run here was refused by the permission gate (expected, not a fault)".into(),
                    ));
                } else {
                    let msg: String = e.to_string().chars().take(90).collect();
                    rows.push(("newest transcript".into(), false, msg))
                }
            }
        },
    }

    let width = rows.iter().map(|(l, _, _)| l.len()).max().unwrap_or(0);
    let mut lines = vec!["agy bridge status".to_string()];
    let mut all_ok = true;
    for (label, ok, detail) in &rows {
        if !ok {
            all_ok = false;
        }
        lines.push(format!(
            "  {:<width$}  [{}] {}",
            label,
            if *ok { "ok" } else { "!!" },
            detail,
            width = width
        ));
    }
    lines.push(format!(
        "Overall: {}",
        if all_ok { "OK" } else { "PROBLEMS FOUND" }
    ));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn denied_job(conv_id: &str) -> JobRecord {
        JobRecord {
            status: "failed".into(),
            conversation_id: Some(conv_id.into()),
            result: Some(
                "a tool was soft-denied in headless mode (agy 1.1.3+ permission gate): \
                 run_command(cargo publish --dry-run) needs the \"command\" permission."
                    .into(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn denied_conversation_is_not_flagged_a_problem() {
        let jobs = vec![denied_job("abc123")];
        assert!(conv_was_denied("abc123", &jobs));
    }

    #[test]
    fn unrelated_conversation_is_not_matched() {
        let jobs = vec![denied_job("abc123")];
        assert!(!conv_was_denied("other-conv", &jobs));
    }

    #[test]
    fn a_failed_run_for_some_other_reason_is_not_treated_as_a_denial() {
        let mut jobs = vec![denied_job("abc123")];
        jobs[0].result = Some("timeout waiting for response".into());
        assert!(!conv_was_denied("abc123", &jobs));
    }

    #[test]
    fn a_running_job_with_the_same_conv_id_is_not_a_denial() {
        let mut jobs = vec![denied_job("abc123")];
        jobs[0].status = "running".into();
        assert!(!conv_was_denied("abc123", &jobs));
    }
}
