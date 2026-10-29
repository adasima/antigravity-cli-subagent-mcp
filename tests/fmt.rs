//! `cargo test` is the pass/fail command for every change, so formatting drift fails here
//! instead of piling up until CI runs. Skipped when rustfmt is not installed.

use std::process::Command;

#[test]
fn the_crate_is_formatted() {
    let out = match Command::new(env!("CARGO"))
        .args(["fmt", "--all", "--", "--check"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
    {
        Ok(out) => out,
        Err(_) => return,
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("not installed") || stderr.contains("no such command") {
        return;
    }
    assert!(
        out.status.success(),
        "run `cargo fmt --all`:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}
