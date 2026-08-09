#![allow(dead_code)] // Fuzzy matching reserved for future TUI search
/// Fuzzy match: the query characters must appear in order in the target.
/// Separators (underscores, hyphens, spaces) are ignored in both query and target.
///
/// Examples:
/// - "deep" matches "DeepSeek API" → true
/// - "bailiancoding" matches "BAILIAN_CODING_PLAN_API_KEY" → true
/// - "qwen" matches "qwen3.7-plus-bailian-lp" → true
/// - "xyz" matches "deepseek" → false
pub fn fuzzy_match(query: &str, target: &str) -> bool {
    if query.is_empty() {
        return true;
    }

    // Normalize: lowercase, remove separators
    let q: Vec<char> = query
        .to_lowercase()
        .chars()
        .filter(|c| !is_separator(*c))
        .collect();
    let t: Vec<char> = target
        .to_lowercase()
        .chars()
        .filter(|c| !is_separator(*c))
        .collect();

    if q.is_empty() {
        return true;
    }
    if q.len() > t.len() {
        return false;
    }

    // Character-by-character ordered match
    let mut ti = 0;
    for qc in &q {
        while ti < t.len() && &t[ti] != qc {
            ti += 1;
        }
        if ti >= t.len() {
            return false;
        }
        ti += 1;
    }
    true
}

/// Score a fuzzy match (higher = better match).
/// - Exact substring: 100
/// - Consecutive character matches: 80+
/// - Separated/gapped matches: score decreases with gap size
pub fn fuzzy_score(query: &str, target: &str) -> i32 {
    if query.is_empty() {
        return 0;
    }

    let q: Vec<char> = query
        .to_lowercase()
        .chars()
        .filter(|c| !is_separator(*c))
        .collect();
    let t: Vec<char> = target.to_lowercase().chars().collect();

    // First: check exact substring (with separators stripped from both)
    let t_stripped: String = target
        .to_lowercase()
        .chars()
        .filter(|c| !is_separator(*c))
        .collect();
    let q_stripped: String = q.iter().collect();
    if t_stripped.contains(&q_stripped) {
        return 200; // excellent contiguous match
    }

    // Character-by-character ordered match with gap penalty
    if q.is_empty() || q.len() > t.len() {
        return -1;
    }

    let mut score = 100i32;
    let mut ti = 0;
    let mut last_match = 0;
    for (matches, qc) in q.iter().enumerate() {
        while ti < t.len() && &t[ti] != qc {
            ti += 1;
        }
        if ti >= t.len() {
            return -1; // no match
        }
        let gap = ti - last_match;
        if gap > 1 {
            score -= gap as i32 * 2; // penalty for gap
        }
        if matches == 0 && ti > 0 && is_separator(t[ti - 1]) {
            score += 5; // bonus for first char after separator
        }
        last_match = ti;
        ti += 1;
    }

    score.max(0)
}

fn is_separator(c: char) -> bool {
    c == '_' || c == '-' || c == ' ' || c == '.'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match() {
        assert!(fuzzy_match("DeepSeek", "DeepSeek API"));
        assert!(fuzzy_match("deepseek", "DeepSeek API"));
        assert!(fuzzy_match("deep", "DeepSeek API"));
    }

    #[test]
    fn test_separator_insensitive() {
        // Underscores are ignored
        assert!(fuzzy_match("BAILIANCODING", "BAILIAN_CODING_PLAN_API_KEY"));
        assert!(fuzzy_match("bailiancoding", "BAILIAN_CODING_PLAN_API_KEY"));
        assert!(fuzzy_match("bailian", "BAILIAN_CODING_PLAN_API_KEY"));

        // Hyphens in target are ignored
        assert!(fuzzy_match("qwen37plus", "qwen3.7-plus-bailian-lp"));
        assert!(fuzzy_match("qwen", "qwen3.7-plus-bailian-lp"));
    }

    #[test]
    fn test_character_order_match() {
        // Characters in order with gaps
        assert!(fuzzy_match("bcp", "bailian-coding-plan")); // b..c..p
        assert!(fuzzy_match("dp", "deepseek")); // d..p
        assert!(fuzzy_match("ds", "deepseek")); // d..s
    }

    #[test]
    fn test_no_match() {
        assert!(!fuzzy_match("xyz", "deepseek"));
        assert!(!fuzzy_match("openai", "deepseek"));
        // Order matters: "pd" does not match "deepseek" (p comes after d)
        assert!(!fuzzy_match("pd", "deepseek"));
    }

    #[test]
    fn test_scoring() {
        // Exact substring = higher score
        assert!(fuzzy_score("deep", "DeepSeek API") > fuzzy_score("ds", "DeepSeek API"));

        // "bailian" in "BAILIAN_CODING" should score well
        assert!(fuzzy_score("bailian", "BAILIAN_CODING_PLAN_API_KEY") >= 100);

        // Gapped match scores lower
        assert!(
            fuzzy_score("bcp", "bailian-coding-plan")
                < fuzzy_score("bailian", "bailian-coding-plan")
        );
    }
}
