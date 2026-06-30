//! Slash-command parser. Turns a user input line into either a (name, args)
//! pair or "this is not a command".

#[derive(Debug, PartialEq)]
pub enum ParsedInput<'a> {
    Command { name: &'a str, args: &'a str },
    NotCommand,
}

pub fn parse(input: &str) -> ParsedInput<'_> {
    let trimmed = input.trim_start();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return ParsedInput::NotCommand;
    };
    let mut iter = rest.splitn(2, char::is_whitespace);
    let name = iter.next().unwrap_or("");
    let args = iter.next().unwrap_or("").trim();
    if name.is_empty() {
        ParsedInput::NotCommand
    } else {
        ParsedInput::Command { name, args }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! cmd {
        ($name:expr, $args:expr) => {
            ParsedInput::Command {
                name: $name,
                args: $args,
            }
        };
    }

    #[test]
    fn parse_help_no_args() {
        assert_eq!(parse("/help"), cmd!("help", ""));
    }

    #[test]
    fn parse_command_with_args() {
        assert_eq!(
            parse("/translate hello world"),
            cmd!("translate", "hello world")
        );
    }

    #[test]
    fn parse_preserves_internal_whitespace_and_trims_outer() {
        assert_eq!(
            parse("/translate   hello   world  "),
            cmd!("translate", "hello   world")
        );
    }

    #[test]
    fn parse_strips_leading_whitespace() {
        assert_eq!(parse("  /help"), cmd!("help", ""));
    }

    #[test]
    fn parse_lone_slash_is_not_command() {
        assert_eq!(parse("/"), ParsedInput::NotCommand);
    }

    #[test]
    fn parse_slash_with_only_whitespace_is_not_command() {
        assert_eq!(parse("/  "), ParsedInput::NotCommand);
    }

    #[test]
    fn parse_empty_string_is_not_command() {
        assert_eq!(parse(""), ParsedInput::NotCommand);
    }

    #[test]
    fn parse_plain_text_is_not_command() {
        assert_eq!(parse("hello"), ParsedInput::NotCommand);
    }

    #[test]
    fn parse_double_slash_yields_unknown_name() {
        // The dispatcher will report this as "unknown command: //cmd".
        // Parser only reports the lexical split.
        assert_eq!(parse("//cmd"), cmd!("/cmd", ""));
    }
}
