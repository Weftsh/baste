//! `${{ }}` templates and `if:` conditions.

use serde_json::Value;

use crate::analysis::contains_status_function;
use crate::eval::eval_checked;
use crate::{parse, Env, Error, Expr};

const OPEN: &str = "${{";

/// A piece of a template.
enum Segment<'t> {
    Text(&'t str),
    /// The text between `${{` and `}}`.
    Expr(&'t str),
}

/// Splits a template into literal text and `${{ }}` blocks. After an
/// unterminated `${{` it yields one error and stops.
struct Segments<'t> {
    src: &'t str,
    pos: usize,
}

impl<'t> Segments<'t> {
    fn new(src: &'t str) -> Self {
        Segments { src, pos: 0 }
    }
}

impl<'t> Iterator for Segments<'t> {
    type Item = Result<Segment<'t>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = &self.src[self.pos..];
        if rest.is_empty() {
            return None;
        }
        match rest.find(OPEN) {
            Some(0) => {
                let open = self.pos;
                let body = open + OPEN.len();
                match find_close(self.src, body) {
                    Some(close) => {
                        self.pos = close + 2;
                        Some(Ok(Segment::Expr(&self.src[body..close])))
                    }
                    None => {
                        self.pos = self.src.len();
                        Some(Err(Error::parse_at(
                            "Unterminated expression: '${{' without a matching '}}'",
                            open,
                            self.src,
                        )))
                    }
                }
            }
            Some(n) => {
                self.pos += n;
                Some(Ok(Segment::Text(&rest[..n])))
            }
            None => {
                self.pos = self.src.len();
                Some(Ok(Segment::Text(rest)))
            }
        }
    }
}

/// Finds the `}}` that closes an expression whose body starts at `body`,
/// skipping over single-quoted string literals (a `''` escape toggles out of
/// and back into the string, so it needs no special handling). Returns the
/// offset of the first `}`.
fn find_close(src: &str, body: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut in_string = false;
    for i in body..bytes.len() {
        match bytes[i] {
            b'\'' => in_string = !in_string,
            b'}' if !in_string && i > body && bytes[i - 1] == b'}' => return Some(i - 1),
            _ => {}
        }
    }
    None
}

/// Parses the body of a `${{ }}` block. Error positions are relative to the
/// trimmed body.
fn parse_body(body: &str) -> Result<Expr, Error> {
    parse(body.trim())
}

/// If `text`, ignoring surrounding whitespace, is exactly one `${{ }}` block,
/// returns its body.
fn single_expression(text: &str) -> Option<&str> {
    let text = text.trim();
    let body = text.strip_prefix(OPEN)?;
    let close = find_close(text, OPEN.len())?;
    (close + 2 == text.len()).then(|| &body[..close - OPEN.len()])
}

enum Part<'t> {
    Text(&'t str),
    Expr(Expr),
}

/// Splits and parses a whole template before anything is evaluated.
fn parse_template(template: &str) -> Result<Vec<Part<'_>>, Error> {
    Segments::new(template)
        .map(|segment| match segment? {
            Segment::Text(text) => Ok(Part::Text(text)),
            Segment::Expr(body) => parse_body(body).map(Part::Expr),
        })
        .collect()
}

fn render(parts: &[Part<'_>], env: &Env) -> Result<String, Error> {
    let mut out = String::new();
    for part in parts {
        match part {
            Part::Text(text) => out.push_str(text),
            Part::Expr(expr) => out.push_str(&eval_checked(expr, env)?.display()),
        }
    }
    Ok(out)
}

/// `true` if `s` contains a `${{`.
pub fn has_expression(s: &str) -> bool {
    s.contains(OPEN)
}

/// Replaces every `${{ expr }}` in `template` with the display string of its
/// value (see [`to_display_string`](crate::to_display_string)). Text outside
/// the blocks is kept verbatim.
///
/// The end of a block is found by skipping string literals, so
/// `${{ format('{{0}}') }}` works. An unterminated `${{` is an
/// [`Error::Parse`] positioned in the template; a syntax error inside a block
/// is positioned in that block's (trimmed) expression.
pub fn interpolate(template: &str, env: &Env) -> Result<String, Error> {
    render(&parse_template(template)?, env)
}

/// If `template`, after trimming whitespace, is exactly one `${{ expr }}`,
/// returns the evaluated value unchanged (arrays, objects, numbers and
/// booleans keep their type). Otherwise returns the [`interpolate`]d string.
/// Strings without `${{` are returned unchanged.
pub fn interpolate_value(template: &str, env: &Env) -> Result<Value, Error> {
    if !has_expression(template) {
        return Ok(Value::String(template.to_owned()));
    }
    if let Some(body) = single_expression(template) {
        return Ok(eval_checked(&parse_body(body)?, env)?.into_value());
    }
    interpolate(template, env).map(Value::String)
}

fn success() -> Expr {
    Expr::Call("success".to_owned(), Vec::new())
}

/// Evaluates an `if:` condition with GitHub's semantics.
///
/// - An empty or whitespace-only condition means `success()`.
/// - If the trimmed text is a single `${{ expr }}`, the wrapper is stripped.
/// - Else if it contains `${{` anywhere, it is interpolated as a string and is
///   true iff the result is non-empty (so `${{ false }} && x` is true).
/// - Else the whole text is an expression.
///
/// Unless one of `success()`, `failure()`, `always()` or `cancelled()`
/// appears somewhere in the condition, it is evaluated as
/// `success() && (condition)`. As on GitHub this also applies to the
/// interpolated form, where the status check runs before the interpolation.
pub fn evaluate_condition(cond: &str, env: &Env) -> Result<bool, Error> {
    let text = cond.trim();
    let expr = if text.is_empty() {
        success()
    } else if let Some(body) = single_expression(text) {
        parse_body(body)?
    } else if has_expression(text) {
        let parts = parse_template(text)?;
        let has_status = parts
            .iter()
            .any(|part| matches!(part, Part::Expr(e) if contains_status_function(e)));
        if !has_status && !eval_checked(&success(), env)?.is_truthy() {
            return Ok(false);
        }
        return Ok(!render(&parts, env)?.is_empty());
    } else {
        parse(text)?
    };

    let expr = if contains_status_function(&expr) {
        expr
    } else {
        Expr::And(Box::new(success()), Box::new(expr))
    };
    Ok(eval_checked(&expr, env)?.is_truthy())
}

/// Calls `f` with every expression in `text` that parses: the `${{ }}` blocks,
/// or in `bare` mode (when the text has no `${{`), the whole text. Syntax
/// errors are skipped; scanning stops at an unterminated `${{`.
pub(crate) fn for_each_expression(text: &str, bare: bool, mut f: impl FnMut(&Expr)) {
    if bare && !has_expression(text) {
        if let Ok(expr) = parse(text) {
            f(&expr);
        }
        return;
    }
    for segment in Segments::new(text) {
        match segment {
            Ok(Segment::Expr(body)) => {
                if let Ok(expr) = parse_body(body) {
                    f(&expr);
                }
            }
            Ok(Segment::Text(_)) => {}
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(src: &str) -> Vec<String> {
        Segments::new(src)
            .map(|s| match s.unwrap() {
                Segment::Text(t) => format!("T:{t}"),
                Segment::Expr(e) => format!("E:{e}"),
            })
            .collect()
    }

    #[test]
    fn splits_templates() {
        assert_eq!(segments("a ${{ b }} c"), vec!["T:a ", "E: b ", "T: c"]);
        assert_eq!(segments("${{x}}${{y}}"), vec!["E:x", "E:y"]);
        assert_eq!(segments("${{ '}}' }}!"), vec!["E: '}}' ", "T:!"]);
        assert_eq!(segments("${{ 'it''s}}' }}"), vec!["E: 'it''s}}' "]);
        assert_eq!(segments("${{ a }}}"), vec!["E: a ", "T:}"]);
        assert_eq!(segments("${{}}"), vec!["E:"]);
        assert_eq!(segments("no expressions"), vec!["T:no expressions"]);
        assert!(segments("").is_empty());
    }

    #[test]
    fn unterminated_blocks() {
        let results: Vec<_> = Segments::new("ok ${{ a }} ${{ '}}").collect();
        assert_eq!(results.len(), 4);
        let err = results.into_iter().last().unwrap().err().unwrap();
        assert!(matches!(err, Error::Parse { pos: 12, .. }), "{err:?}");
    }

    #[test]
    fn detects_single_expressions() {
        assert_eq!(single_expression("${{ a }}"), Some(" a "));
        assert_eq!(single_expression("  ${{ a }}\n"), Some(" a "));
        assert_eq!(single_expression("${{ '}}' }}"), Some(" '}}' "));
        assert_eq!(single_expression("${{ a }} "), Some(" a "));
        assert_eq!(single_expression("${{ a }}${{ b }}"), None);
        assert_eq!(single_expression("x ${{ a }}"), None);
        assert_eq!(single_expression("${{ a }} x"), None);
        assert_eq!(single_expression("${{ a"), None);
    }
}
