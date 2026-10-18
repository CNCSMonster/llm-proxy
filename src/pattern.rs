//! Reusable pattern matching supporting exact match and glob wildcards (`*`, `?`, `[...]`).

use globset::{Glob, GlobMatcher};

/// Returns true if `pattern` contains glob wildcard characters (`*`, `?`, or `[`).
pub fn is_glob_pattern(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[')
}

/// PatternMatcher matches string slices against an exact target or a compiled glob pattern.
#[derive(Debug, Clone)]
pub struct PatternMatcher {
    raw: String,
    matcher: Option<GlobMatcher>,
}

impl PatternMatcher {
    /// Creates a new `PatternMatcher`.
    ///
    /// If `pattern` contains glob metacharacters (`*`, `?`, `[`), it is compiled as a glob.
    /// Returns an error if the glob syntax is invalid.
    pub fn new(pattern: &str) -> Result<Self, String> {
        let matcher = if is_glob_pattern(pattern) {
            let glob =
                Glob::new(pattern).map_err(|e| format!("invalid glob pattern '{pattern}': {e}"))?;
            Some(glob.compile_matcher())
        } else {
            None
        };
        Ok(Self {
            raw: pattern.to_string(),
            matcher,
        })
    }

    /// Returns true if the candidate string matches this pattern.
    pub fn is_match(&self, s: &str) -> bool {
        match &self.matcher {
            Some(m) => m.is_match(s),
            None => self.raw == s,
        }
    }

    /// Returns true if this pattern contains glob metacharacters and was compiled as a glob.
    pub fn is_glob(&self) -> bool {
        self.matcher.is_some()
    }

    /// Returns the raw pattern string.
    #[allow(dead_code)]
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_glob_pattern() {
        assert!(is_glob_pattern("qwen-*"));
        assert!(is_glob_pattern("qwen?"));
        assert!(is_glob_pattern("qwen[1-3]"));
        assert!(!is_glob_pattern("qwen-flash"));
        assert!(!is_glob_pattern("deepseek-v3"));
    }

    #[test]
    fn test_exact_match() {
        let m = PatternMatcher::new("qwen-flash").expect("valid");
        assert!(!m.is_glob());
        assert_eq!(m.raw(), "qwen-flash");
        assert!(m.is_match("qwen-flash"));
        assert!(!m.is_match("qwen-flash-2"));
        assert!(!m.is_match("other"));
    }

    #[test]
    fn test_glob_star() {
        let m = PatternMatcher::new("qwen-*").expect("valid");
        assert!(m.is_glob());
        assert!(m.is_match("qwen-flash"));
        assert!(m.is_match("qwen-turbo-max"));
        assert!(m.is_match("qwen-"));
        assert!(!m.is_match("deepseek-v3"));
        assert!(!m.is_match("prefix-qwen-flash"));
    }

    #[test]
    fn test_glob_question_mark() {
        let m = PatternMatcher::new("qwen3.?").expect("valid");
        assert!(m.is_glob());
        assert!(m.is_match("qwen3.8"));
        assert!(m.is_match("qwen3.0"));
        assert!(!m.is_match("qwen3.88"));
        assert!(!m.is_match("qwen3."));
    }

    #[test]
    fn test_glob_character_set() {
        let m = PatternMatcher::new("model-[ab]").expect("valid");
        assert!(m.is_glob());
        assert!(m.is_match("model-a"));
        assert!(m.is_match("model-b"));
        assert!(!m.is_match("model-c"));
    }

    #[test]
    fn test_invalid_glob_syntax() {
        let err = PatternMatcher::new("model-[").unwrap_err();
        assert!(err.contains("invalid glob pattern"));
        assert!(err.contains("model-["));
    }
}
