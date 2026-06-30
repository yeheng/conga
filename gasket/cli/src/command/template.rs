//! Day-1 template renderer. Single-placeholder string replacement.

pub fn render(template: &str, user_input: &str) -> String {
    template.replace("{{user_input}}", user_input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_user_input() {
        assert_eq!(render("X {{user_input}} Y", "hello"), "X hello Y");
    }

    #[test]
    fn substitutes_multiple_occurrences() {
        assert_eq!(render("{{user_input}}-{{user_input}}", "a"), "a-a");
    }

    #[test]
    fn unknown_placeholder_passes_through() {
        assert_eq!(render("{{foo}}", "ignored"), "{{foo}}");
    }

    #[test]
    fn empty_user_input_yields_empty_substitution() {
        assert_eq!(
            render("Translate: {{user_input}} done.", ""),
            "Translate:  done."
        );
    }
}
