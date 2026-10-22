//! Structure-aware chunking — pure function.

/// One output chunk. `ordinal` is the 0-based position within the document.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub ordinal: usize,
    pub content: String,
}

pub fn chunk(text: &str, target_chars: usize, overlap_chars: usize) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut cur = String::new();
    let mut ordinal = 0usize;

    fn flush(
        cur: &mut String,
        chunks: &mut Vec<Chunk>,
        ordinal: &mut usize,
        overlap: usize,
    ) -> String {
        // trim_end only: the leading edge may be the carried overlap tail,
        // whose exact characters (including a leading space) must survive so
        // the next chunk starts with the exact tail of this one.
        let content = cur.trim_end().to_string();
        cur.clear();
        if content.is_empty() {
            return String::new();
        }
        // The overlap tail must come from the same trimmed content that is
        // emitted, so the next chunk starts with the exact tail of this one.
        let tail = tail_chars(&content, overlap);
        chunks.push(Chunk {
            ordinal: *ordinal,
            content,
        });
        *ordinal += 1;
        tail
    }

    for para in text.split("\n\n") {
        let is_heading = para
            .lines()
            .next()
            .map(|l| l.starts_with("# ") || l.starts_with("## "))
            .unwrap_or(false);

        for piece in soft_split(para, target_chars) {
            if !cur.is_empty()
                && (is_heading || cur.chars().count() + piece.chars().count() + 2 > target_chars)
            {
                // Emit current chunk; carry the overlap tail into the next.
                let tail = flush(&mut cur, &mut chunks, &mut ordinal, overlap_chars);
                if !tail.is_empty() {
                    cur.push_str(&tail);
                    cur.push_str("\n\n");
                }
            }
            if !cur.is_empty() {
                cur.push_str("\n\n");
            }
            cur.push_str(&piece);
        }
    }
    flush(&mut cur, &mut chunks, &mut ordinal, overlap_chars);
    chunks
}

/// Split an oversize paragraph at sentence boundaries, then whitespace, then
/// hard character cuts — never exceeding `target` unless a single
/// non-splittable run is itself longer.
fn soft_split(para: &str, target: usize) -> Vec<String> {
    if para.chars().count() <= target {
        return vec![para.to_string()];
    }
    let chars: Vec<char> = para.chars().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = (start + target).min(chars.len());
        if end == chars.len() {
            out.push(chars[start..end].iter().collect());
            break;
        }
        let mut cut = end;
        for i in (start..end).rev() {
            if "。！？!?；;".contains(chars[i]) {
                cut = i + 1;
                break;
            }
        }
        if cut == end {
            for i in (start..end).rev() {
                if chars[i].is_whitespace() {
                    cut = i + 1;
                    break;
                }
            }
        }
        out.push(chars[start..cut].iter().collect());
        start = cut;
    }
    out
}

/// Last `n` chars of `s`, char-boundary safe.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if n == 0 || count == 0 {
        return String::new();
    }
    let skip = count.saturating_sub(n);
    s.chars().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(chunks: &[Chunk]) -> Vec<&str> {
        chunks.iter().map(|c| c.content.as_str()).collect()
    }

    #[test]
    fn short_text_is_one_chunk() {
        let out = chunk("hello world", 100, 10);
        assert_eq!(contents(&out), vec!["hello world"]);
        assert_eq!(out[0].ordinal, 0);
    }

    #[test]
    fn ordinals_are_contiguous() {
        let text = (0..20)
            .map(|i| format!("paragraph {i} {}", "x".repeat(40)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = chunk(&text, 150, 20);
        assert!(out.len() > 1);
        for (i, c) in out.iter().enumerate() {
            assert_eq!(c.ordinal, i);
        }
    }

    #[test]
    fn heading_forces_new_chunk() {
        let text = format!("{}aa bb\n\n# Heading\n\ncc dd", "intro ".repeat(10));
        let out = chunk(&text, 200, 0);
        assert!(out.len() >= 2);
        let joined = contents(&out).join("\x1f");
        assert!(
            joined.contains("\x1f# Heading"),
            "heading must start a new chunk"
        );
    }

    #[test]
    fn chunks_respect_target() {
        let text = (0..30)
            .map(|i| format!("para-{i} {}", "y".repeat(30)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let target = 200usize;
        let out = chunk(&text, target, 30);
        for c in &out {
            assert!(
                c.content.chars().count() <= target + 30,
                "chunk too big: {}",
                c.content.chars().count()
            );
        }
    }

    #[test]
    fn oversize_paragraph_soft_splits() {
        let one = "z".repeat(500);
        let out = chunk(&one, 120, 20);
        assert!(
            out.len() >= 4,
            "500 chars / 120 target must split, got {}",
            out.len()
        );
    }

    #[test]
    fn overlap_repeats_tail() {
        let text = (0..10)
            .map(|i| format!("para-{i} {}", "w".repeat(60)))
            .collect::<Vec<_>>()
            .join("\n\n");
        let out = chunk(&text, 150, 30);
        assert!(out.len() > 1);
        let tail: String = out[0]
            .content
            .chars()
            .rev()
            .take(30)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert!(
            out[1].content.starts_with(&tail),
            "second chunk should start with overlap tail"
        );
    }

    #[test]
    fn overlap_tail_is_exact_after_whitespace_soft_split() {
        // Plain English words: no 。！？!?；; punctuation, so soft_split must
        // cut at whitespace and the resulting piece can end in a space.
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo \
                    lima mike november oscar papa quebec romeo sierra tango uniform \
                    victor whiskey xray yankee zulu";
        let out = chunk(text, 120, 20);
        assert!(out.len() > 1, "must produce multiple chunks");
        for pair in out.windows(2) {
            let prev: Vec<char> = pair[0].content.chars().collect();
            let n = 20.min(prev.len());
            let tail: String = prev[prev.len() - n..].iter().collect();
            assert!(
                pair[1].content.starts_with(&tail),
                "chunk {} must start with the exact last {n} chars of chunk {}",
                pair[1].ordinal,
                pair[0].ordinal
            );
        }
    }

    #[test]
    fn utf8_never_panics() {
        let text = "你好世界。".repeat(100);
        let out = chunk(&text, 37, 9);
        assert!(!out.is_empty());
        for c in &out {
            assert!(c.content.chars().count() > 0);
        }
    }

    // --- flush (a nested helper of `chunk`, exercised through it) ---

    #[test]
    fn flush_emits_no_chunk_for_a_whitespace_only_buffer() {
        let out = chunk("   \t  ", 100, 10);
        assert!(
            out.is_empty(),
            "a buffer that trims to empty must not produce a chunk: {out:?}"
        );
    }

    #[test]
    fn flush_trims_the_trailing_whitespace_of_the_emitted_chunk() {
        let out = chunk("a  \n\nb  ", 100, 0);
        assert_eq!(contents(&out), vec!["a  \n\nb"]);
    }

    #[test]
    fn flush_carries_the_overlap_tail_of_the_trimmed_content() {
        let text = "alpha bravo charlie delta echo foxtrot golf";
        let out = chunk(text, 20, 5);
        assert!(out.len() > 1, "must produce multiple chunks");
        let first: Vec<char> = out[0].content.chars().collect();
        let tail: String = first[first.len() - 5..].iter().collect();
        assert!(
            out[1].content.starts_with(&tail),
            "the next chunk must start with the tail of the previous one"
        );
    }

    // --- soft_split ---

    #[test]
    fn soft_split_keeps_a_short_paragraph_whole() {
        assert_eq!(soft_split("hello", 10), vec!["hello".to_string()]);
    }

    #[test]
    fn soft_split_keeps_a_paragraph_of_exactly_target_whole() {
        let para = "a".repeat(10);
        assert_eq!(soft_split(&para, 10), vec![para]);
    }

    #[test]
    fn soft_split_prefers_a_sentence_boundary() {
        let para = "第一句。第二句。第三句。";
        let out = soft_split(para, 5);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], "第一句。");
        assert_eq!(out[1], "第二句。");
        assert_eq!(out[2], "第三句。");
    }

    #[test]
    fn soft_split_falls_back_to_a_word_boundary() {
        let para = "alpha bravo charlie delta echo foxtrot";
        let out = soft_split(para, 12);
        assert_eq!(out.concat(), para, "no content may be lost");
        for piece in &out[..out.len() - 1] {
            assert!(
                piece.ends_with(' '),
                "a word must not be cut in half: {piece:?}"
            );
        }
    }

    #[test]
    fn soft_split_hard_cuts_when_no_boundary_exists() {
        let para = "x".repeat(25);
        let out = soft_split(&para, 10);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].chars().count(), 10);
        assert_eq!(out[2].chars().count(), 5);
        assert_eq!(out.concat(), para);
    }

    #[test]
    fn soft_split_handles_unicode_without_panicking() {
        let para = "你好世界。".repeat(10);
        let out = soft_split(&para, 7);
        assert_eq!(out.concat(), para);
        for piece in &out {
            assert!(piece.chars().count() <= 7, "piece too big: {piece:?}");
        }
    }

    #[test]
    fn soft_split_of_an_empty_paragraph_yields_one_empty_piece() {
        assert_eq!(soft_split("", 10), vec![String::new()]);
    }

    // --- tail_chars ---

    #[test]
    fn tail_chars_returns_empty_for_a_zero_length_tail() {
        assert_eq!(tail_chars("hello", 0), "");
    }

    #[test]
    fn tail_chars_returns_empty_for_an_empty_input() {
        assert_eq!(tail_chars("", 5), "");
    }

    #[test]
    fn tail_chars_returns_the_last_n_chars() {
        assert_eq!(tail_chars("hello", 3), "llo");
    }

    #[test]
    fn tail_chars_returns_the_whole_string_when_n_exceeds_the_length() {
        assert_eq!(tail_chars("hello", 99), "hello");
    }

    #[test]
    fn tail_chars_is_char_boundary_safe() {
        assert_eq!(tail_chars("你好世界", 2), "世界");
    }
}
