use std::path::Path;
use std::process::Stdio;

/// Snapshot of a git workspace's dirty state: `porcelain` status lines keyed by
/// path. Taken before and after a run so the diff is *what the run actually
/// changed*, not what it says it changed.
pub type GitSnapshot = std::collections::HashMap<String, String>;

/// `git status --porcelain` for `ws`, or None if it isn't a git work tree (or git
/// isn't installed). None is the "no ground truth available" signal — callers fall
/// back to the transcript rather than reporting an empty diff as "changed nothing".
pub fn git_snapshot(ws: &Path) -> Option<GitSnapshot> {
    let mut cmd = std::process::Command::new("git");
    // `core.quotepath` defaults to true, which renders every non-ASCII byte of a
    // path as an octal escape (`\346\227\245`). That went straight into the `wrote:`
    // footer as mojibake AND made `metadata()` miss on every non-ASCII untracked
    // path, so edits to those files never showed up as a change.
    cmd.args(["-c", "core.quotepath=false"])
        .arg("-C")
        .arg(ws)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .stdin(Stdio::null());
    // Invariant #1: windowless server — a shelled-out child must not flash a console.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None; // not a repo, or git missing
    }
    Some(parse_porcelain_with(
        &String::from_utf8_lossy(&out.stdout),
        |path| {
            // mtime AND length: mtime alone misses a same-length rewrite inside the
            // filesystem's timestamp granularity, and length alone misses an edit that
            // happens to preserve the size. One `metadata()` per listed path (tracked or
            // not) — bounded by the paths porcelain just walked anyway. Without this on a
            // TRACKED path, a file already dirty (` M`) before the run and still ` M`
            // after it compares equal even when the run rewrote it — a real run whose only
            // edit was to an already-modified file got a false "may have punted" nudge.
            let meta = std::fs::metadata(ws.join(path)).ok()?;
            let d = meta
                .modified()
                .ok()?
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .ok()?;
            Some(format!(
                "{}.{:09}:{}",
                d.as_secs(),
                d.subsec_nanos(),
                meta.len()
            ))
        },
    ))
}

/// Parse `git status --porcelain` into path → status-code. Pure, so the rename and
/// quoting cases are testable without a repo.
#[cfg(test)]
pub(crate) fn parse_porcelain(raw: &str) -> GitSnapshot {
    parse_porcelain_with(raw, |_| None)
}

/// Decode git's C-style quoting of a porcelain path. git quotes whenever the path
/// holds a space, a quote, a control char — or, with `core.quotepath` on, any
/// non-ASCII byte, escaped in octal. `git_snapshot` turns that flag off, but the
/// quoting itself survives for the other cases (and an odd git could ignore the
/// flag), so the escapes are decoded here rather than assumed away. Pure: the
/// bytes are rebuilt and read back as UTF-8, which is what the path actually is.
pub(crate) fn unquote_porcelain_path(raw: &str) -> String {
    let inner = match raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => inner,
        None => return raw.to_string(),
    };
    let b = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&c) = b.get(i) else { break };
        match c {
            b'n' => {
                out.push(b'\n');
                i += 1;
            }
            b't' => {
                out.push(b'\t');
                i += 1;
            }
            b'r' => {
                out.push(b'\r');
                i += 1;
            }
            b'0'..=b'7' => {
                // Up to three octal digits per escape — one raw byte, not one char:
                // a multi-byte character arrives as several consecutive escapes.
                let mut v: u32 = 0;
                let mut n = 0;
                while n < 3 {
                    match b.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            v = v * 8 + u32::from(d - b'0');
                            i += 1;
                            n += 1;
                        }
                        _ => break,
                    }
                }
                out.push(v as u8);
            }
            // `\"` and `\\`, plus anything unrecognized: the escaped byte itself.
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse porcelain status output using an injected fingerprint source, called for
/// every listed path (tracked or not). Without it, a path whose status code is
/// unchanged between two snapshots reads as untouched — true for an edit to an
/// already-`??` untracked file, and equally true for a rewrite of a tracked file
/// that was already dirty (` M` before, ` M` after): the code alone can't tell
/// those apart from a no-op, only the fingerprint can.
pub(crate) fn parse_porcelain_with<F>(raw: &str, mut stat: F) -> GitSnapshot
where
    F: FnMut(&str) -> Option<String>,
{
    raw.lines()
        .filter(|l| l.len() > 3)
        .map(|l| {
            let (code, rest) = l.split_at(2);
            let rest = rest.trim_start();
            // A rename reads `R  old -> new`; the new path is what exists now.
            let path = rest.rsplit(" -> ").next().unwrap_or(rest);
            let cleaned_path = unquote_porcelain_path(path);
            let trimmed_code = code.trim();
            // A deleted path has no metadata to fingerprint (`stat` returns None) and
            // keeps its bare code — the code change alone (present -> absent) is
            // already the signal there.
            let stored_code = match stat(&cleaned_path) {
                Some(fp) => format!("{trimmed_code}:{fp}"),
                None => trimmed_code.to_string(),
            };
            (cleaned_path, stored_code)
        })
        .collect()
}

/// Paths whose git status changed between two snapshots — the ground truth for
/// "what did this run write". Sorted for a stable footer.
///
/// Pure and unit-tested: a path is reported when it appears, disappears, or
/// changes status code (e.g. ` M` → `M `, or untracked `??` → added).
pub fn git_changed_paths(before: &GitSnapshot, after: &GitSnapshot) -> Vec<String> {
    let mut out: Vec<String> = after
        .iter()
        .filter(|(p, code)| before.get(*p) != Some(code))
        .map(|(p, _)| p.clone())
        .collect();
    // A path that was dirty before and is clean now (the run reverted or staged it)
    // is also a change the caller should see.
    out.extend(before.keys().filter(|p| !after.contains_key(*p)).cloned());
    out.sort();
    out.dedup();
    out
}

/// Cap for the `wrote:` line-count/EOL notes (docs/feedback-2026-09-15 P1). A
/// generated/vendored blob makes both scans slow and the note itself
/// meaningless ("12000→12050 lines" tells the reader nothing); 5 MB
/// comfortably covers real source and doc files.
const NOTE_MAX_BYTES: usize = 5 * 1024 * 1024;

/// Whether `bytes` looks binary: a NUL byte in the first 8 KB — the same
/// heuristic git itself uses to decide whether a file is diffable at all.
/// Good enough to skip a meaningless line-count/EOL note on a binary blob.
pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

/// Whether `bytes` is small and text-y enough for a `wrote:` note to bother
/// with — see `NOTE_MAX_BYTES`/`looks_binary`.
fn within_note_limits(bytes: &[u8]) -> bool {
    bytes.len() <= NOTE_MAX_BYTES && !looks_binary(bytes)
}

/// Number of lines in `bytes`: every `\n` counts one, plus one more for a
/// trailing partial line with no final newline — the intuitive "how many
/// lines are in this file" count, not `wc -l`'s raw newline tally (a file
/// ending without `\n` still has one). Empty content is 0 lines.
pub(crate) fn count_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    if bytes.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}

/// The `wrote:` line-count note for one path, from its line count at HEAD
/// (`before` — None means no HEAD blob, i.e. a new file) and in the working
/// tree now (`after` — None means unreadable, oversized, or binary; see
/// `within_note_limits`, applied by the caller before counting). `after:
/// None` omits the note entirely rather than guessing at a number; `before:
/// None` with `after: Some` is the ordinary "new file" case, not a skip.
pub(crate) fn line_count_note(before: Option<usize>, after: Option<usize>) -> Option<String> {
    match (before, after) {
        (None, Some(m)) => Some(format!("new, {m} line{}", if m == 1 { "" } else { "s" })),
        (Some(n), Some(m)) => Some(format!("{n}→{m} lines")),
        (_, None) => None,
    }
}

/// Dominant line ending in `bytes`, or None when it can't be called — no line
/// endings at all, or an exact tie between CRLF and bare LF (an ambiguous mix
/// is not worth reporting as "the" ending). A bare `\n` is only counted when
/// it wasn't immediately preceded by `\r` (that pair is CRLF, not LF).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Eol {
    Crlf,
    Lf,
}

impl Eol {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Eol::Crlf => "CRLF",
            Eol::Lf => "LF",
        }
    }
}

pub(crate) fn dominant_eol(bytes: &[u8]) -> Option<Eol> {
    let mut crlf = 0usize;
    let mut lf_only = 0usize;
    let mut prev_cr = false;
    for &b in bytes {
        if b == b'\n' {
            if prev_cr {
                crlf += 1;
            } else {
                lf_only += 1;
            }
        }
        prev_cr = b == b'\r';
    }
    match crlf.cmp(&lf_only) {
        std::cmp::Ordering::Greater => Some(Eol::Crlf),
        std::cmp::Ordering::Less => Some(Eol::Lf),
        std::cmp::Ordering::Equal => None, // no line endings, or an exact tie
    }
}

/// `git show HEAD:<rel>` for a workspace-relative, forward-slashed path — the
/// pre-run content for the `wrote:` line-count/EOL notes. None when there is
/// no HEAD blob (new file, not a repo, or git missing): the "no ground truth"
/// signal callers treat as "nothing to compare against", same posture as
/// `git_snapshot`.
fn head_blob(ws: &Path, rel: &str) -> Option<Vec<u8>> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(ws)
        .arg("show")
        .arg(format!("HEAD:{rel}"))
        .stdin(Stdio::null());
    // Invariant #1: windowless server — a shelled-out child must not flash a console.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// The paths (of `rels`) whose line endings git normalizes (`text` set or
/// `auto`): there an EOL flip never reaches a commit, and the HEAD blob (LF) is
/// not what the working tree held before the run (it may be CRLF — this repo's
/// `install.ps1` is `i/lf w/crlf`), so comparing the two only raises false
/// alarms (D7). Empty when git is missing or fails: keep the note, not hide it.
pub fn git_normalized_paths(ws: &Path, rels: &[String]) -> std::collections::HashSet<String> {
    if rels.is_empty() {
        return Default::default();
    }
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(ws)
        .args(["check-attr", "-z", "text", "--"])
        .args(rels)
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match cmd.output() {
        Ok(out) if out.status.success() => parse_check_attr_text(&out.stdout),
        _ => Default::default(),
    }
}

/// `git check-attr -z text` output is `<path>\0text\0<value>\0` per path.
fn parse_check_attr_text(out: &[u8]) -> std::collections::HashSet<String> {
    let fields: Vec<String> = out
        .split(|b| *b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect();
    fields
        .chunks_exact(3)
        .filter(|c| c[2] == "set" || c[2] == "auto")
        .map(|c| c[0].clone())
        .collect()
}

/// The `wrote:` footer notes for one path (docs/feedback-2026-09-15 P1): a
/// line-count delta and, separately, whether its dominant line ending flipped
/// between HEAD and the working tree. One HEAD-blob read and one working-tree
/// read feed both pure decisions (`line_count_note`/`dominant_eol`) so a path
/// needing both notes doesn't pay for the read twice. `rel` is
/// workspace-relative and forward-slashed (git's own separator).
pub struct PathNote {
    pub line_count: Option<String>,
    /// `(before, after)` EOL labels ("CRLF"/"LF"), present only when the
    /// dominant ending actually changed on both sides being determinable.
    pub eol_change: Option<(&'static str, &'static str)>,
}

pub fn wrote_path_note(ws: &Path, rel: &str) -> PathNote {
    let before = head_blob(ws, rel).filter(|b| within_note_limits(b));
    // Check the working-tree file's size via metadata before reading it, so an
    // oversized file (already excluded by `within_note_limits`) is never fully
    // loaded into memory just to be discarded.
    let full_path = ws.join(rel);
    let after = std::fs::metadata(&full_path)
        .ok()
        .filter(|m| m.len() <= NOTE_MAX_BYTES as u64)
        .and_then(|_| std::fs::read(&full_path).ok())
        .filter(|b| within_note_limits(b));
    let line_count = line_count_note(
        before.as_deref().map(count_lines),
        after.as_deref().map(count_lines),
    );
    let eol_change = match (
        before.as_deref().and_then(dominant_eol),
        after.as_deref().and_then(dominant_eol),
    ) {
        (Some(b), Some(a)) if b != a => Some((b.label(), a.label())),
        _ => None,
    };
    PathNote {
        line_count,
        eol_change,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_parses_the_shapes_git_actually_emits() {
        let snap = parse_porcelain(
            " M src/main.rs\n\
             ?? docs/new.md\n\
             R  old/a.rs -> new/a.rs\n\
             A  \"has space.txt\"\n",
        );
        assert_eq!(snap.get("src/main.rs").map(String::as_str), Some("M"));
        assert_eq!(snap.get("docs/new.md").map(String::as_str), Some("??"));
        // A rename reports the path that exists NOW, not the one that's gone.
        assert_eq!(snap.get("new/a.rs").map(String::as_str), Some("R"));
        assert!(!snap.contains_key("old/a.rs"));
        // Paths with spaces arrive quoted; the quotes aren't part of the path.
        assert_eq!(snap.get("has space.txt").map(String::as_str), Some("A"));
    }

    #[test]
    fn quoted_paths_are_decoded_back_to_the_real_name() {
        // What `core.quotepath` (git's default) does to a non-ASCII name. Left as the
        // literal escapes it used to put `\346\227\245…` in the `wrote:` footer and
        // made every metadata() lookup on the path miss.
        let snap = parse_porcelain(
            "?? \"\\346\\227\\245\\346\\234\\254\\350\\252\\236.md\"\n M \"a\\\"b.rs\"\n",
        );
        assert_eq!(snap.get("日本語.md").map(String::as_str), Some("??"));
        // A literal quote inside the name is escaped, not a delimiter.
        assert_eq!(snap.get("a\"b.rs").map(String::as_str), Some("M"));
        // An unquoted path is returned untouched — the common case must not be rewritten.
        assert_eq!(unquote_porcelain_path("src/main.rs"), "src/main.rs");
        // With quotepath off, git still quotes a name with a space but leaves the
        // non-ASCII bytes raw; both halves of that must survive.
        assert_eq!(unquote_porcelain_path("\"日本 語.md\""), "日本 語.md");
    }

    #[test]
    fn git_diff_reports_appeared_changed_and_vanished_paths() {
        let before = parse_porcelain(" M keep.rs\n M staged.rs\n?? gone.txt\n");
        let after = parse_porcelain(" M keep.rs\nM  staged.rs\n?? fresh.rs\nMM both.rs\n");
        let changed = git_changed_paths(&before, &after);
        // fresh.rs and both.rs appeared; gone.txt is no longer dirty (the run staged
        // or reverted it). keep.rs is untouched. staged.rs only moved from unstaged
        // to staged — the code is trimmed, so a pure `git add` is deliberately NOT a
        // write: the punt nudge must still fire on a run that only staged things.
        assert_eq!(changed, vec!["both.rs", "fresh.rs", "gone.txt"]);
        // A run that changed nothing must report exactly nothing — that emptiness is
        // what the punt nudge treats as proof, so it can't be approximate.
        assert!(git_changed_paths(&before, &before).is_empty());
    }

    #[test]
    fn untracked_edits_show_up_as_a_status_change() {
        let raw = "?? note.txt\n";
        let before = parse_porcelain_with(raw, |_| Some("a".to_string()));
        let after = parse_porcelain_with(raw, |_| Some("b".to_string()));
        assert_eq!(git_changed_paths(&before, &after), vec!["note.txt"]);
    }

    #[test]
    fn identical_fingerprints_leave_both_tracked_and_untracked_paths_untouched() {
        // `stat` now runs for every path, not just `??` ones — a tracked file's
        // fingerprint has to agree too before the pair reads as unchanged.
        let raw = "?? note.txt\n M src/main.rs\n";
        let before = parse_porcelain_with(raw, |_| Some("a".to_string()));
        let after = parse_porcelain_with(raw, |_| Some("a".to_string()));
        assert!(git_changed_paths(&before, &after).is_empty());
        assert_eq!(before.get("src/main.rs").map(String::as_str), Some("M:a"));
        assert_eq!(before.get("note.txt").map(String::as_str), Some("??:a"));
    }

    /// The bug this closes: a tracked file already dirty (` M`) before the run and
    /// still ` M` after it used to compare equal on the code alone, so a run whose
    /// only edit was to an already-modified file reported a false "may have
    /// punted" nudge. The fingerprint is what proves the rewrite happened.
    #[test]
    fn a_rewrite_of_an_already_dirty_tracked_file_is_detected() {
        let raw = " M src/agy/transcript.rs\n";
        let before = parse_porcelain_with(raw, |_| Some("t1".to_string()));
        let after = parse_porcelain_with(raw, |_| Some("t2".to_string()));
        assert_eq!(
            git_changed_paths(&before, &after),
            vec!["src/agy/transcript.rs"]
        );
    }

    /// A deleted path has nothing left to `stat` — it keeps its bare code, and the
    /// code's own appear/disappear transition (checked in
    /// `git_diff_reports_appeared_changed_and_vanished_paths`) is what reports it.
    #[test]
    fn a_deleted_path_has_no_metadata_and_keeps_its_bare_code() {
        let snap = parse_porcelain_with(" D gone.rs\n", |_| None);
        assert_eq!(snap.get("gone.rs").map(String::as_str), Some("D"));
    }

    #[test]
    fn looks_binary_flags_a_nul_in_the_first_8kb_only() {
        assert!(!looks_binary(b"plain text, no nul bytes"));
        assert!(looks_binary(b"lots of \x00 binary junk"));
        // A NUL past the 8KB window doesn't count — this is a cheap heuristic,
        // not a full scan.
        let mut far = vec![b'a'; 8200];
        far.push(0);
        assert!(!looks_binary(&far));
    }

    #[test]
    fn count_lines_counts_a_trailing_partial_line() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"one line, no trailing newline"), 1);
        assert_eq!(count_lines(b"a\nb\nc\n"), 3);
        assert_eq!(count_lines(b"a\nb\nc"), 3);
        assert_eq!(count_lines(b"\n"), 1);
    }

    #[test]
    fn line_count_note_distinguishes_new_from_changed_and_omits_on_unreadable_after() {
        assert_eq!(
            line_count_note(None, Some(78)),
            Some("new, 78 lines".to_string())
        );
        assert_eq!(
            line_count_note(None, Some(1)),
            Some("new, 1 line".to_string())
        );
        assert_eq!(
            line_count_note(Some(1662), Some(1589)),
            Some("1662→1589 lines".to_string())
        );
        // Unreadable/oversized/binary working-tree content: no note, not a guess.
        assert_eq!(line_count_note(Some(10), None), None);
        assert_eq!(line_count_note(None, None), None);
    }

    #[test]
    fn check_attr_text_keeps_only_paths_git_normalizes() {
        let out = b"install.ps1\0text\0auto\0a.md\0text\0set\0b.bin\0text\0unset\0c.x\0text\0unspecified\0";
        let got = parse_check_attr_text(out);
        assert!(got.contains("install.ps1") && got.contains("a.md"));
        assert!(!got.contains("b.bin") && !got.contains("c.x"));
        assert!(parse_check_attr_text(b"").is_empty());
    }

    #[test]
    fn dominant_eol_picks_the_majority_and_gives_up_on_a_tie_or_no_endings() {
        assert_eq!(dominant_eol(b"a\r\nb\r\nc\r\n"), Some(Eol::Crlf));
        assert_eq!(dominant_eol(b"a\nb\nc\n"), Some(Eol::Lf));
        // A tie (including the "no line endings at all" tie, 0 == 0) can't be
        // called either way.
        assert_eq!(dominant_eol(b"a\r\nb\n"), None);
        assert_eq!(dominant_eol(b"no newlines here"), None);
        // Majority CRLF with one stray bare LF still reads as CRLF.
        assert_eq!(dominant_eol(b"a\r\nb\r\nc\n"), Some(Eol::Crlf));
    }
}
