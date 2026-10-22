//! Text cleaning — pure function, no I/O.

/// Normalize: strip BOM, CRLF/CR → LF, trim trailing whitespace per line,
/// collapse runs of blank lines to a single blank line, trim leading/trailing
/// blank lines. Markdown syntax is preserved as-is.
pub fn clean(input: &str) -> String {
    let s = input.strip_prefix('\u{feff}').unwrap_or(input);
    let s = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0;
    for line in s.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blanks += 1;
            if blanks <= 1 {
                out.push('\n');
            }
        } else {
            blanks = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_bom_and_normalizes_newlines() {
        let out = clean("\u{feff}a\r\nb\rc");
        assert_eq!(out, "a\nb\nc");
    }

    #[test]
    fn trims_line_trailing_whitespace() {
        assert_eq!(clean("a  \nb\t\n"), "a\nb");
    }

    #[test]
    fn collapses_blank_runs_to_one() {
        assert_eq!(clean("a\n\n\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn trims_leading_and_trailing_blank_lines() {
        assert_eq!(clean("\n\na\n\n"), "a");
    }

    #[test]
    fn keeps_markdown_structure() {
        let md = "# Title\n\n- item\n\n## Sub\n";
        assert_eq!(clean(md), "# Title\n\n- item\n\n## Sub");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(clean(""), "");
    }

    #[test]
    fn whitespace_only_input_becomes_empty() {
        assert_eq!(clean("   \t  "), "");
        assert_eq!(clean("\n\n \n"), "");
    }

    #[test]
    fn keeps_html_and_entities_verbatim() {
        // Markdown (and any embedded HTML) is preserved as-is: the cleaner
        // only normalizes whitespace, it never rewrites markup.
        let html = "<script>alert(1)</script> <style>p{}</style> &amp; &#65;";
        assert_eq!(clean(html), html);
    }

    #[test]
    fn collapses_a_long_run_of_blank_lines() {
        let input = format!("a{}b", "\n".repeat(50));
        assert_eq!(clean(&input), "a\n\nb");
    }

    #[test]
    fn strips_only_the_first_bom() {
        assert_eq!(clean("\u{feff}\u{feff}a"), "\u{feff}a");
    }

    #[test]
    fn keeps_leading_indentation_but_trims_trailing_whitespace() {
        assert_eq!(clean("  indented  \n\talso indented\t"), "  indented\n\talso indented");
    }

    #[test]
    fn normalizes_mixed_line_endings() {
        assert_eq!(clean("a\r\nb\nc\rd"), "a\nb\nc\nd");
    }
}
