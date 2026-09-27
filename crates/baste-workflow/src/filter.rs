//! GitHub's filter pattern syntax for branches, tags and paths.
//!
//! `*` matches anything but `/`, `**` matches anything, `?` and `+` apply to
//! the preceding character, `[...]` is a character class, and a leading `!`
//! negates a pattern. In a list, the last matching pattern wins.

use regex::Regex;

/// A compiled filter pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    negated: bool,
    regex: Regex,
}

impl Pattern {
    pub fn new(pattern: &str) -> Result<Pattern, String> {
        let (negated, body) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };
        let regex = Regex::new(&to_regex(body))
            .map_err(|e| format!("invalid filter pattern '{pattern}': {e}"))?;
        Ok(Pattern { negated, regex })
    }

    pub fn is_negated(&self) -> bool {
        self.negated
    }

    /// Whether the pattern body (ignoring negation) matches `s`.
    pub fn matches(&self, s: &str) -> bool {
        self.regex.is_match(s)
    }
}

fn to_regex(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' if chars.get(i + 1) == Some(&'*') => {
                if chars.get(i + 2) == Some(&'/') {
                    out.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    out.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => out.push_str("[^/]*"),
            '?' | '+' => out.push(c),
            '[' => {
                // Copy a character class through, escaping regex specials inside it.
                if let Some(end) = chars[i + 1..].iter().position(|&ch| ch == ']') {
                    let class: String = chars[i + 1..i + 1 + end].iter().collect();
                    out.push('[');
                    for ch in class.chars() {
                        if ch == '\\' || ch == '[' {
                            out.push('\\');
                        }
                        out.push(ch);
                    }
                    out.push(']');
                    i += end + 2;
                    continue;
                }
                out.push_str("\\[");
            }
            _ => out.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    out.push('$');
    out
}

/// A list of patterns where the last matching one decides.
#[derive(Debug, Clone)]
pub struct PatternList {
    patterns: Vec<Pattern>,
}

impl PatternList {
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Result<PatternList, String> {
        Ok(PatternList {
            patterns: patterns
                .iter()
                .map(|p| Pattern::new(p.as_ref()))
                .collect::<Result<_, _>>()?,
        })
    }

    /// Whether `s` is included: the last pattern that matches it is positive.
    pub fn includes(&self, s: &str) -> bool {
        let mut included = false;
        for p in &self.patterns {
            if p.matches(s) {
                included = !p.is_negated();
            }
        }
        included
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, s: &str) -> bool {
        Pattern::new(p).unwrap().matches(s)
    }

    #[test]
    fn github_cheat_sheet() {
        assert!(m("feature/*", "feature/my-branch"));
        assert!(!m("feature/*", "feature/your/branch"));
        assert!(m("feature/**", "feature/your/branch"));
        assert!(m("main", "main"));
        assert!(!m("main", "mainline"));
        assert!(m("*", "main"));
        assert!(!m("*", "releases/v1"));
        assert!(m("**", "releases/v1"));
        assert!(m("v2*", "v2.0.1"));
        assert!(m("v[12].[0-9]+.[0-9]+", "v1.10.1"));
        assert!(!m("v[12].[0-9]+.[0-9]+", "v3.0.0"));
        assert!(m("*.js", "app.js"));
        assert!(!m("*.js", "js/index.js"));
        assert!(m("**.js", "js/index.js"));
        assert!(m("docs/**", "docs/a/b.md"));
        assert!(m("**/docs/**", "docs/a.md"));
        assert!(m("**/docs/**", "x/y/docs/a.md"));
        assert!(m("**/*.md", "README.md"));
        assert!(m("**/*.md", "a/b/c.md"));
        assert!(m("*.jsx?", "page.js"));
        assert!(m("*.jsx?", "page.jsx"));
        assert!(m("a.b", "a.b"));
        assert!(!m("a.b", "axb"));
    }

    #[test]
    fn last_match_wins() {
        let l = PatternList::new(&["releases/**", "!releases/**-alpha"]).unwrap();
        assert!(l.includes("releases/v1"));
        assert!(!l.includes("releases/v1-alpha"));
        assert!(!l.includes("main"));
    }
}
