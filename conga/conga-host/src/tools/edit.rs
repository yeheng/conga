//! `edit` tool — replace text in a file, written atomically.
//!
//! Exact match is tried first. If the literal `old_text` isn't found, a fuzzy
//! fallback tolerates whitespace and curly-quote differences (models frequently
//! mangle indentation, trailing whitespace, or quote style). The fuzzy match is
//! mapped back to the original byte range so the replacement lands exactly
//! where intended. In all cases `old_text` must resolve to exactly one location.

use std::sync::Arc;

use conga::types::tool::{RiskLevel, ToolCallCtx, ToolDefinition, ToolResult};
use conga::ContentBlock;

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "edit".into(),
        label: "Edit".into(),
        description: "Replace text in a file using one or more edits. Each old_text must be unique (whitespace/quote differences tolerated). All edits apply together: if any one fails to match, the file is left untouched.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": { "type": "string" },
                            "new_text": { "type": "string" }
                        },
                        "required": ["old_text", "new_text"]
                    }
                }
            },
            "required": ["path", "edits"]
        }),
        risk: RiskLevel::Medium,
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

/// One hunk: replace `old_text` with `new_text`.
struct Hunk<'a> {
    old_text: &'a str,
    new_text: &'a str,
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, conga::error::ToolError> {
    if ctx.aborted() {
        return Ok(ToolResult::error("aborted".to_string()));
    }
    let path = ctx.args["path"]
        .as_str()
        .ok_or_else(|| conga::error::ToolError::Message("path is required".into()))?;
    let edits = ctx.args["edits"]
        .as_array()
        .ok_or_else(|| conga::error::ToolError::Message("edits array is required".into()))?;
    if edits.is_empty() {
        return Ok(ToolResult::error("edits must not be empty".to_string()));
    }
    let mut hunks: Vec<Hunk> = Vec::with_capacity(edits.len());
    for (i, e) in edits.iter().enumerate() {
        let old_text = e["old_text"].as_str().ok_or_else(|| {
            conga::error::ToolError::Message(format!("edits[{i}].old_text is required"))
        })?;
        let new_text = e["new_text"].as_str().ok_or_else(|| {
            conga::error::ToolError::Message(format!("edits[{i}].new_text is required"))
        })?;
        hunks.push(Hunk { old_text, new_text });
    }

    let full = match super::resolve_within_cwd(&ctx.ctx.cwd, path) {
        Ok(p) => p,
        Err(msg) => return Ok(ToolResult::error(msg)),
    };
    let original = tokio::fs::read_to_string(&full).await?;

    // Phase 1: locate every hunk against the ORIGINAL text (exact match
    // first, fuzzy fallback). Any failure aborts the whole edit — the file
    // must not change unless every hunk lands.
    let mut located: Vec<(LocatedRange, &str)> = Vec::with_capacity(hunks.len());
    for (i, h) in hunks.iter().enumerate() {
        let range = match locate_unique(&original, h.old_text) {
            Ok(r) => r,
            Err(msg) => {
                return Ok(ToolResult::error(format!(
                    "edits[{i}] {msg}; no changes applied to {path}"
                )))
            }
        };
        located.push((range, h.new_text));
    }

    // Phase 2: apply all hunks back-to-front so earlier ranges stay valid.
    located.sort_by_key(|(r, _)| r.start);
    let mut updated = original.clone();
    for (range, new_text) in located.iter().rev() {
        updated.replace_range(range.start..range.end, new_text);
    }

    // Atomic write via temp file + rename. The tmp name adds a unique uuid
    // before the suffix (suffix, not `with_extension`, which would drop the
    // original extension and let `Cargo.toml`/`Cargo.lock` collide): concurrent
    // writers to the same target never share a tmp file. Same directory as the
    // target, so `rename` stays atomic within one filesystem.
    let mut tmp_os = full.clone().into_os_string();
    tmp_os.push(format!(".{}.conga-tmp", uuid::Uuid::new_v4()));
    let tmp = std::path::PathBuf::from(tmp_os);
    let outcome = async {
        tokio::fs::write(&tmp, &updated).await?;
        tokio::fs::rename(&tmp, &full).await
    }
    .await;
    if outcome.is_err() {
        // Best effort: never leave our own tmp behind on a failed edit.
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    outcome?;

    let matches: Vec<&str> = located
        .iter()
        .map(|(r, _)| if r.exact { "exact" } else { "fuzzy" })
        .collect();
    Ok(ToolResult {
        content: vec![ContentBlock::text(format!(
            "edited {} ({} hunk{})",
            path,
            hunks.len(),
            if hunks.len() == 1 { "" } else { "s" }
        ))],
        details: serde_json::json!({"path": path, "matches": matches}),
        is_error: false,
    })
}

/// A located hunk: byte range in the original plus whether it was an exact
/// match (fuzzy matches report differently in the result details).
struct LocatedRange {
    start: usize,
    end: usize,
    exact: bool,
}

/// Find `old_text` in `original` exactly once. Exact substring first; on
/// zero hits, the whitespace/quote-tolerant fuzzy scan. `Err` carries the
/// reason (not found / not unique).
fn locate_unique(original: &str, old_text: &str) -> Result<LocatedRange, String> {
    let count = original.matches(old_text).count();
    if count == 1 {
        let start = original.find(old_text).expect("count == 1 implies find");
        return Ok(LocatedRange {
            start,
            end: start + old_text.len(),
            exact: true,
        });
    }
    if count > 1 {
        return Err(format!("old_text appears {count} times; it must be unique"));
    }
    match fuzzy_locate(original, old_text) {
        Fuzzy::None => Err("old_text not found (exact or fuzzy match)".into()),
        Fuzzy::Many(n) => Err(format!(
            "old_text matches {n} times after whitespace/quote normalization; it must be unique"
        )),
        Fuzzy::Unique(start, end) => Ok(LocatedRange {
            start,
            end,
            exact: false,
        }),
    }
}

/// Result of a fuzzy (whitespace- and quote-tolerant) search for `old_text`.
enum Fuzzy {
    /// No match even after normalization.
    None,
    /// Exactly one match; byte range (start inclusive, end exclusive) in the
    /// original text.
    Unique(usize, usize),
    /// Multiple matches after normalization; still ambiguous.
    Many(usize),
}

/// Locate `old_text` in `original` ignoring whitespace and treating curly quotes
/// as their ASCII equivalents. Returns [`Fuzzy::Unique`] with the byte range in
/// the *original* string when there is exactly one normalized match.
fn fuzzy_locate(original: &str, old_text: &str) -> Fuzzy {
    let (norm_file, map) = normalize_fuzzy(original);
    let (norm_old, _) = normalize_fuzzy(old_text);
    if norm_old.is_empty() {
        return Fuzzy::None;
    }

    // Collect non-overlapping match start offsets (byte offsets within norm_file).
    let mut starts: Vec<usize> = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = norm_file[from..].find(&norm_old) {
        let abs = from + rel;
        starts.push(abs);
        from = abs + norm_old.len();
    }
    match starts.len() {
        0 => Fuzzy::None,
        1 => {
            let start_byte = starts[0];
            let end_byte = start_byte + norm_old.len();
            // Convert norm_file byte offsets -> char indices -> original byte offsets.
            let start_char = norm_file[..start_byte].chars().count();
            let end_char = norm_file[..end_byte].chars().count(); // exclusive
            let orig_start = map[start_char];
            let last_off = map[end_char - 1];
            let last_len = original[last_off..]
                .chars()
                .next()
                .map_or(0, |c| c.len_utf8());
            Fuzzy::Unique(orig_start, last_off + last_len)
        }
        n => Fuzzy::Many(n),
    }
}

/// Normalize for fuzzy matching: drop all whitespace and convert curly/smart
/// quotes to straight ASCII quotes. Returns the normalized string plus a map
/// from each normalized char's index to the byte offset of its source char in
/// the input (so a normalized match can be mapped back to the original).
fn normalize_fuzzy(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len());
    for (byte_off, ch) in s.char_indices() {
        if ch.is_whitespace() {
            continue;
        }
        let n = match ch {
            '\u{2018}' | '\u{2019}' => '\'', // ‘ ’
            '\u{201C}' | '\u{201D}' => '"',  // “ ”
            c => c,
        };
        out.push(n);
        map.push(byte_off);
    }
    (out, map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::types::tool::ToolContext;
    use std::sync::atomic::AtomicBool;

    async fn run(args: serde_json::Value, cwd: &std::path::Path) -> ToolResult {
        let t = tool();
        (t.execute)(ToolCallCtx {
            tool_call_id: "x".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: cwd.to_path_buf(),
                env: Default::default(),
                session_id: "s".into(),
                state_dir: cwd.to_path_buf(),
            },
        })
        .await
        .unwrap()
    }

    fn one_edit(old: &str, new: &str) -> serde_json::Value {
        serde_json::json!({"old_text": old, "new_text": new})
    }

    #[tokio::test]
    async fn replaces_unique_match() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "foo bar baz")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "edits": [one_edit("bar", "QUX")]}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error);
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("f.txt"))
                .await
                .unwrap(),
            "foo QUX baz"
        );
        assert_eq!(r.details["matches"][0], "exact");
    }

    #[tokio::test]
    async fn multiple_hunks_apply_together() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "alpha beta gamma")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "edits": [
                one_edit("alpha", "ONE"),
                one_edit("gamma", "THREE"),
            ]}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error, "details: {:?}", r.details);
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("f.txt"))
                .await
                .unwrap(),
            "ONE beta THREE"
        );
    }

    #[tokio::test]
    async fn one_bad_hunk_leaves_file_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "alpha beta gamma")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "edits": [
                one_edit("alpha", "ONE"),
                one_edit("zzz", "NOPE"),
            ]}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("f.txt"))
                .await
                .unwrap(),
            "alpha beta gamma",
            "a failed hunk must abort the whole edit"
        );
    }

    #[tokio::test]
    async fn mixed_exact_and_fuzzy_hunks() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "foo    bar    baz")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "edits": [
                one_edit("foo bar baz", "X"),
            ]}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error);
        assert_eq!(r.details["matches"][0], "fuzzy");
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("f.txt"))
                .await
                .unwrap(),
            "X"
        );
    }

    #[tokio::test]
    async fn errors_on_multiple_matches() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "x x x")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "edits": [one_edit("x", "y")]}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn errors_on_missing_match() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "abc")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "edits": [one_edit("zzz", "y")]}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn errors_on_empty_edits() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "abc")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "edits": []}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn fuzzy_matches_smart_quotes() {
        // File uses curly quotes; old_text uses straight quotes.
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "say \u{201C}hello\u{201D} now")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "edits": [one_edit("say \"hello\" now", "hi")]}),
            tmp.path(),
        )
        .await;
        assert!(!r.is_error);
        assert_eq!(r.details["matches"][0], "fuzzy");
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("f.txt"))
                .await
                .unwrap(),
            "hi"
        );
    }

    #[tokio::test]
    async fn fuzzy_preserves_surrounding_text() {
        // Fuzzy match must only replace the matched span, leaving the rest intact.
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("f.txt"), "HEAD  middle  TAIL")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "edits": [one_edit("middle", "M")]}),
            tmp.path(),
        )
        .await;
        // Exact match exists here, so it should be exact, not fuzzy.
        assert_eq!(r.details["matches"][0], "exact");
        assert_eq!(
            tokio::fs::read_to_string(tmp.path().join("f.txt"))
                .await
                .unwrap(),
            "HEAD  M  TAIL"
        );
    }

    #[tokio::test]
    async fn fuzzy_errors_on_multiple_after_norm() {
        let tmp = tempfile::tempdir().unwrap();
        // Two whitespace-normalized occurrences of "a b".
        tokio::fs::write(tmp.path().join("f.txt"), "a  b and a  b")
            .await
            .unwrap();
        let r = run(
            serde_json::json!({"path": "f.txt", "edits": [one_edit("a b", "z")]}),
            tmp.path(),
        )
        .await;
        assert!(r.is_error);
    }

    // ---- locate_unique: exact / fuzzy / ambiguity / not-found ----

    /// `LocatedRange` carries no `Debug` impl, so `unwrap_err` is not
    /// available on `Result<LocatedRange, String>` — unwrap the error by
    /// hand instead.
    fn locate_err(original: &str, old_text: &str) -> String {
        match locate_unique(original, old_text) {
            Ok(_) => panic!("expected {old_text:?} not to match {original:?}"),
            Err(e) => e,
        }
    }

    /// Happy path: one exact hit -> the byte range of that hit, flagged as
    /// an exact match.
    #[test]
    fn locate_unique_returns_exact_range_for_single_match() {
        let original = "alpha beta gamma";
        let r = locate_unique(original, "beta").expect("single match");
        assert!(r.exact, "must be flagged as an exact match");
        assert_eq!(r.start, 6);
        assert_eq!(r.end, 10);
        assert_eq!(&original[r.start..r.end], "beta");
    }

    /// Error path: several exact hits -> the count is reported so the model
    /// can disambiguate.
    #[test]
    fn locate_unique_rejects_multiple_matches() {
        let err = locate_err("x x x", "x");
        assert!(err.contains("appears 3 times"), "{err}");
        assert!(err.contains("must be unique"), "{err}");
    }

    /// Error path: neither an exact nor a normalized hit.
    #[test]
    fn locate_unique_reports_not_found() {
        let err = locate_err("abc", "zzz");
        assert!(err.contains("not found"), "{err}");
    }

    /// The fuzzy fallback still reports a range into the ORIGINAL text, so
    /// the replacement lands where the author meant it.
    #[test]
    fn locate_unique_falls_back_to_fuzzy_range_in_original() {
        let original = "foo    bar    baz";
        let r = locate_unique(original, "foo bar baz").expect("fuzzy match");
        assert!(!r.exact, "must be flagged as a fuzzy match");
        assert_eq!(&original[r.start..r.end], "foo    bar    baz");
    }

    /// Error path: the fuzzy fallback itself is ambiguous.
    #[test]
    fn locate_unique_reports_ambiguity_after_normalization() {
        let err = locate_err("a  b and a  b", "a b");
        assert!(
            err.contains("after whitespace/quote normalization"),
            "{err}"
        );
        assert!(err.contains("must be unique"), "{err}");
    }

    /// Boundary: an empty old_text matches at every position, so it must be
    /// refused instead of replacing the whole file.
    #[test]
    fn locate_unique_rejects_empty_old_text() {
        let err = locate_err("abc", "");
        assert!(err.contains("appears"), "{err}");
    }

    /// Curly quotes in the file are normalized for matching, but the
    /// reported range still covers the original curly-quote bytes.
    #[test]
    fn locate_unique_maps_curly_quote_span_back_to_original() {
        let original = "say \u{201C}hello\u{201D} now";
        let r = locate_unique(original, "say \"hello\" now").expect("fuzzy match");
        assert!(!r.exact);
        assert_eq!(&original[r.start..r.end], "say \u{201C}hello\u{201D} now");
    }

    // ---- fuzzy_locate ----

    #[test]
    fn fuzzy_locate_returns_none_for_empty_pattern() {
        assert!(matches!(fuzzy_locate("abc", ""), Fuzzy::None));
        assert!(matches!(fuzzy_locate("", "abc"), Fuzzy::None));
    }

    #[test]
    fn fuzzy_locate_returns_many_for_repeated_normalized_match() {
        match fuzzy_locate("a  b and a  b", "a b") {
            Fuzzy::Many(n) => assert_eq!(n, 2, "both normalized hits count"),
            _ => panic!("expected two fuzzy matches"),
        }
    }

    #[test]
    fn fuzzy_locate_maps_middle_span_back_to_original_bytes() {
        let original = "HEAD  middle  TAIL";
        match fuzzy_locate(original, "middle") {
            Fuzzy::Unique(start, end) => {
                assert_eq!(&original[start..end], "middle");
            }
            _ => panic!("expected a unique fuzzy match"),
        }
    }

    #[test]
    fn fuzzy_locate_matches_across_unicode() {
        let original = "你 好 世界";
        match fuzzy_locate(original, "你好世界") {
            Fuzzy::Unique(start, end) => {
                assert_eq!(&original[start..end], "你 好 世界");
            }
            _ => panic!("expected a unique fuzzy match"),
        }
    }

    // ---- normalize_fuzzy ----

    #[test]
    fn normalize_fuzzy_drops_whitespace_and_maps_source_offsets() {
        let (norm, map) = normalize_fuzzy("a b\tc\nd");
        assert_eq!(norm, "abcd");
        assert_eq!(map, vec![0, 2, 4, 6]);
    }

    #[test]
    fn normalize_fuzzy_converts_curly_quotes_to_ascii() {
        let (norm, map) = normalize_fuzzy("\u{2018}a\u{2019} \u{201C}b\u{201D}");
        assert_eq!(norm, "'a'\"b\"");
        assert_eq!(map.len(), 6, "one source offset per normalized char");
        // The map points at the byte offset of each SOURCE char.
        assert_eq!(map[0], 0, "the opening quote");
        assert_eq!(map[1], 3, "the letter after a 3-byte quote");
    }

    #[test]
    fn normalize_fuzzy_handles_empty_and_multibyte_input() {
        let (norm, map) = normalize_fuzzy("");
        assert_eq!(norm, "");
        assert!(map.is_empty());

        let (norm, map) = normalize_fuzzy("你 好");
        assert_eq!(norm, "你好");
        assert_eq!(map, vec![0, 4], "byte offsets of the source chars");
    }
}
