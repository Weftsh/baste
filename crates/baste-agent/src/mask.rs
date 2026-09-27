//! Secret masking for everything the agent reports.

use std::sync::RwLock;

/// Replaces registered secret values with `***`.
#[derive(Default)]
pub struct Masker {
    values: RwLock<Vec<String>>,
}

impl Masker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a value. Multi-line values are also masked line by line,
    /// since output is reported one line at a time.
    pub fn add(&self, value: &str) {
        let mut values = self.values.write().unwrap();
        let mut push = |v: &str| {
            let v = v.trim_end_matches('\r');
            // Masking one- or two-character values would shred ordinary output.
            if v.trim().len() >= 3 && !values.iter().any(|x| x == v) {
                values.push(v.to_string());
            }
        };
        push(value);
        if value.contains('\n') {
            for line in value.lines() {
                push(line);
            }
        }
        // Longest first, so a secret that contains another is fully masked.
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    }

    pub fn mask(&self, s: &str) -> String {
        let values = self.values.read().unwrap();
        let mut out = s.to_string();
        for v in values.iter() {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), "***");
            }
        }
        out
    }

    /// Whether `s` contains any registered value.
    pub fn contains_secret(&self, s: &str) -> bool {
        self.values
            .read()
            .unwrap()
            .iter()
            .any(|v| s.contains(v.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_longest_first_and_lines() {
        let m = Masker::new();
        m.add("abc");
        m.add("abcdef");
        m.add("line-one\nline-two");
        assert_eq!(m.mask("x abcdef y abc"), "x *** y ***");
        assert_eq!(m.mask("got line-two"), "got ***");
        assert!(m.contains_secret("..abc.."));
        m.add("ab");
        assert_eq!(m.mask("ab"), "ab");
    }
}
