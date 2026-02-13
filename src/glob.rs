/// Hand-rolled fnmatch-style glob matching.
/// Supports `*` (any sequence) and `?` (any single char).
/// Both pattern and text are lowercased before comparison (case-insensitive).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let text = text.to_lowercase();
    glob_match_bytes(pattern.as_bytes(), text.as_bytes())
}

fn glob_match_bytes(pattern: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi = usize::MAX;
    let mut star_ti = 0;

    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == text[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_matches_anything() {
        assert!(glob_match("*", "anything at all"));
    }

    #[test]
    fn question_mark_matches_single_char() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
    }

    #[test]
    fn star_at_domain_matches() {
        assert!(glob_match("*@example.com", "user@example.com"));
    }

    #[test]
    fn star_at_domain_does_not_match_other() {
        assert!(!glob_match("*@example.com", "user@other.com"));
    }

    #[test]
    fn prefix_star_matches() {
        assert!(glob_match("noreply@*", "noreply@anything.com"));
    }

    #[test]
    fn exact_match() {
        assert!(glob_match("foo@bar.com", "foo@bar.com"));
    }

    #[test]
    fn case_insensitive() {
        assert!(glob_match("*@Example.COM", "USER@example.com"));
    }

    #[test]
    fn empty_pattern_matches_empty_string() {
        assert!(glob_match("", ""));
    }

    #[test]
    fn empty_pattern_does_not_match_nonempty() {
        assert!(!glob_match("", "something"));
    }

    #[test]
    fn nested_star_wildcard() {
        assert!(glob_match("*@*.google.com", "cal@mail.google.com"));
    }

    #[test]
    fn no_match() {
        assert!(!glob_match("specific@email.com", "other@email.com"));
    }
}
