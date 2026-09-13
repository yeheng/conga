//! Loop-hygiene guard (borrowed from dsh `guard/`): advisory note when the
//! model makes the exact same tool call three times in a row.
//!
//! Only calls which reach execution are observed — calls rejected earlier
//! (hook Block, malformed args, unknown tool) do not count toward the streak.

/// Tracks only the previous call's (tool, args) — any change resets the
/// streak. No window, no map: the simplest structure that answers "is this
/// the third identical call in a row?".
pub struct RepeatGuard {
    last: Option<(String, String)>,
    count: u32,
}

impl Default for RepeatGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl RepeatGuard {
    pub fn new() -> Self {
        Self {
            last: None,
            count: 0,
        }
    }

    /// Record a call; returns the current consecutive-repeat count (1 = first).
    pub fn observe(&mut self, tool: &str, args_key: &str) -> u32 {
        let same = self
            .last
            .as_ref()
            .is_some_and(|(t, a)| t == tool && a == args_key);
        self.count = if same { self.count + 1 } else { 1 };
        self.last = Some((tool.to_string(), args_key.to_string()));
        self.count
    }
}

/// The note appended to the tool result, fired exactly once per streak.
pub fn repeat_advisory(count: u32) -> Option<String> {
    (count == 3).then(|| {
        "note: this is the third identical call in a row — the result is unlikely \
     to change. If it failed twice, change the arguments or approach."
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_consecutive_identical_calls() {
        let mut g = RepeatGuard::new();
        assert_eq!(g.observe("bash", r#"{"command":"ls"}"#), 1);
        assert_eq!(g.observe("bash", r#"{"command":"ls"}"#), 2);
        assert_eq!(g.observe("bash", r#"{"command":"ls"}"#), 3);
        // A change on either side resets the streak.
        assert_eq!(g.observe("bash", r#"{"command":"pwd"}"#), 1);
        assert_eq!(g.observe("read", r#"{"command":"pwd"}"#), 1);
        // Returning to the original combo starts a fresh streak too.
        assert_eq!(g.observe("bash", r#"{"command":"ls"}"#), 1);
    }

    #[test]
    fn advisory_fires_only_at_three() {
        assert!(repeat_advisory(1).is_none());
        assert!(repeat_advisory(2).is_none());
        let msg = repeat_advisory(3).unwrap();
        assert!(msg.contains("identical"), "{msg}");
        assert!(repeat_advisory(4).is_none());
        assert!(repeat_advisory(9).is_none());
    }
}
