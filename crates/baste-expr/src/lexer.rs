//! Tokenizer, following the rules of the GitHub Actions runner's lexer.

use crate::value::parse_number;
use crate::Error;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TokenKind {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    Comma,
    Star,
    Not,
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Number(f64),
    String(String),
    /// A keyword: a named value, function name, property name, or one of the
    /// literals `null`, `true`, `false`, `NaN` and `Infinity`. The parser
    /// decides which from the surrounding tokens.
    Ident,
}

#[derive(Debug, Clone)]
pub(crate) struct Token<'s> {
    pub(crate) kind: TokenKind,
    /// The token's source text.
    pub(crate) text: &'s str,
    /// Byte offset of the token in the source.
    pub(crate) pos: usize,
}

type Lexed = Result<(TokenKind, usize), Error>;

/// Splits `src` into tokens.
pub(crate) fn tokenize(src: &str) -> Result<Vec<Token<'_>>, Error> {
    let mut tokens: Vec<Token<'_>> = Vec::new();
    let mut pos = 0;
    while let Some(c) = src[pos..].chars().next() {
        if c.is_whitespace() {
            pos += c.len_utf8();
            continue;
        }
        let (kind, end) = match c {
            '(' => (TokenKind::LParen, pos + 1),
            ')' => (TokenKind::RParen, pos + 1),
            '[' => (TokenKind::LBracket, pos + 1),
            ']' => (TokenKind::RBracket, pos + 1),
            ',' => (TokenKind::Comma, pos + 1),
            '*' => (TokenKind::Star, pos + 1),
            '\'' => lex_string(src, pos)?,
            '!' | '<' | '>' | '=' | '&' | '|' => lex_operator(src, pos)?,
            '.' if !number_may_start_after(tokens.last()) => (TokenKind::Dot, pos + 1),
            '.' | '-' | '+' | '0'..='9' => lex_number(src, pos)?,
            _ => lex_keyword(src, pos)?,
        };
        tokens.push(Token {
            kind,
            text: &src[pos..end],
            pos,
        });
        pos = end;
    }
    Ok(tokens)
}

/// Characters that end a number or keyword token.
fn is_boundary(c: char) -> bool {
    matches!(
        c,
        '(' | ')' | '[' | ']' | ',' | '.' | '!' | '<' | '>' | '=' | '&' | '|'
    ) || c.is_whitespace()
}

/// Whether a `.` following `prev` starts a number such as `.5` rather than
/// being a property dereference.
fn number_may_start_after(prev: Option<&Token<'_>>) -> bool {
    use TokenKind::*;
    match prev {
        None => true,
        Some(t) => matches!(
            t.kind,
            Comma | LParen | LBracket | Not | And | Or | Eq | Ne | Lt | Le | Gt | Ge
        ),
    }
}

/// Returns the offset of the first character at or after `from` for which
/// `keep` is false (or the end of `src`).
fn scan_while(src: &str, from: usize, keep: impl Fn(char) -> bool) -> usize {
    src[from..]
        .char_indices()
        .find(|&(_, c)| !keep(c))
        .map_or(src.len(), |(i, _)| from + i)
}

fn unexpected(src: &str, start: usize, end: usize) -> Error {
    let text = &src[start..end];
    let mut message = format!("Unexpected symbol: '{text}'");
    if text.starts_with('"') {
        message.push_str(". String literals must use single quotes");
    }
    Error::parse_at(message, start, src)
}

/// Reads `'...'`, where `''` is an escaped quote.
fn lex_string(src: &str, start: usize) -> Lexed {
    let body = start + 1;
    let mut value = String::new();
    let mut chars = src[body..].char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '\'' {
            value.push(c);
        } else if chars.next_if(|&(_, c)| c == '\'').is_some() {
            value.push('\'');
        } else {
            return Ok((TokenKind::String(value), body + i + 1));
        }
    }
    Err(Error::parse_at("Unterminated string literal", start, src))
}

fn lex_operator(src: &str, start: usize) -> Lexed {
    let two = match src.get(start..start + 2) {
        Some("==") => Some(TokenKind::Eq),
        Some("!=") => Some(TokenKind::Ne),
        Some("<=") => Some(TokenKind::Le),
        Some(">=") => Some(TokenKind::Ge),
        Some("&&") => Some(TokenKind::And),
        Some("||") => Some(TokenKind::Or),
        _ => None,
    };
    if let Some(kind) = two {
        return Ok((kind, start + 2));
    }
    match src.as_bytes()[start] {
        b'!' => Ok((TokenKind::Not, start + 1)),
        b'<' => Ok((TokenKind::Lt, start + 1)),
        b'>' => Ok((TokenKind::Gt, start + 1)),
        // A lone `=`, `&` or `|`.
        _ => Err(unexpected(
            src,
            start,
            scan_while(src, start + 1, |c| !is_boundary(c)),
        )),
    }
}

/// Reads a number. Like the runner, the token runs to the next boundary
/// character (dots included) and must then parse as a number as a whole, so
/// `-1`, `1.5e3`, `0xff` and `-Infinity` are numbers but `1-2` is an error.
fn lex_number(src: &str, start: usize) -> Lexed {
    let end = scan_while(src, start + 1, |c| c == '.' || !is_boundary(c));
    let n = parse_number(&src[start..end]);
    if n.is_nan() {
        return Err(unexpected(src, start, end));
    }
    Ok((TokenKind::Number(n), end))
}

fn lex_keyword(src: &str, start: usize) -> Lexed {
    let first = src[start..].chars().next().map_or(1, char::len_utf8);
    let end = scan_while(src, start + first, |c| !is_boundary(c));
    if !is_legal_keyword(&src[start..end]) {
        return Err(unexpected(src, start, end));
    }
    Ok((TokenKind::Ident, end))
}

/// `[A-Za-z_][A-Za-z0-9_-]*`
fn is_legal_keyword(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        tokenize(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn operators_and_punctuation() {
        assert_eq!(
            kinds("( ) [ ] , * ! && || == != < <= > >="),
            vec![
                LParen, RParen, LBracket, RBracket, Comma, Star, Not, And, Or, Eq, Ne, Lt, Le, Gt,
                Ge
            ]
        );
    }

    #[test]
    fn dot_is_number_or_dereference_depending_on_context() {
        assert_eq!(kinds(".5"), vec![Number(0.5)]);
        assert_eq!(kinds("(.5)"), vec![LParen, Number(0.5), RParen]);
        assert_eq!(kinds("a.b"), vec![Ident, Dot, Ident]);
        assert_eq!(
            kinds("a[0].b"),
            vec![Ident, LBracket, Number(0.0), RBracket, Dot, Ident]
        );
        assert_eq!(kinds("a.*"), vec![Ident, Dot, Star]);
        assert_eq!(kinds("!.5"), vec![Not, Number(0.5)]);
    }

    #[test]
    fn numbers() {
        assert_eq!(kinds("-1"), vec![Number(-1.0)]);
        assert_eq!(kinds("+2.5"), vec![Number(2.5)]);
        assert_eq!(kinds("1e3"), vec![Number(1000.0)]);
        assert_eq!(kinds("-2.99e-2"), vec![Number(-0.0299)]);
        assert_eq!(kinds("0x2A"), vec![Number(42.0)]);
        assert_eq!(kinds("0o17"), vec![Number(15.0)]);
        assert_eq!(kinds("-Infinity"), vec![Number(f64::NEG_INFINITY)]);
        assert!(tokenize("1-2").is_err());
        assert!(tokenize("1.2.3").is_err());
        assert!(tokenize("- 1").is_err());
        assert!(tokenize("-NaN").is_err());
    }

    #[test]
    fn identifiers_may_contain_hyphens() {
        let tokens = tokenize("steps.my-step.outputs").unwrap();
        let texts: Vec<_> = tokens.iter().map(|t| t.text).collect();
        assert_eq!(texts, vec!["steps", ".", "my-step", ".", "outputs"]);
    }

    #[test]
    fn strings() {
        assert_eq!(kinds("'it''s'"), vec![String("it's".into())]);
        assert_eq!(kinds("''"), vec![String(std::string::String::new())]);
        assert_eq!(kinds("'héllo'"), vec![String("héllo".into())]);
        let err = tokenize("x == 'abc").unwrap_err();
        assert!(matches!(err, Error::Parse { pos: 5, .. }), "{err:?}");
    }

    #[test]
    fn illegal_symbols() {
        for src in ["\"main\"", "a = b", "a & b", "a | b", "foo$", "$x", "a.b*"] {
            assert!(tokenize(src).is_err(), "{src}");
        }
        let err = tokenize("x == \"main\"").unwrap_err();
        let Error::Parse { message, pos, .. } = err else {
            panic!("expected a parse error");
        };
        assert_eq!(pos, 5);
        assert!(message.contains("single quotes"), "{message}");
    }

    #[test]
    fn token_positions_are_byte_offsets() {
        let tokens = tokenize("'é' == x").unwrap();
        let positions: Vec<_> = tokens.iter().map(|t| t.pos).collect();
        assert_eq!(positions, vec![0, 5, 8]);
    }
}
