//! Recursive-descent parser with precedence climbing for binary operators.
//!
//! Precedence, loosest first: `||`, `&&`, `==`/`!=`, `<`/`<=`/`>`/`>=`, `!`,
//! then property access, indexing and grouping. Binary operators are left
//! associative.

use std::iter::Peekable;
use std::vec::IntoIter;

use crate::lexer::{tokenize, Token, TokenKind};
use crate::{CmpOp, Error, Expr};

/// Maximum depth of the expression tree. It bounds recursion in the
/// evaluator and tree walks, so hostile input produces an error instead of
/// overflowing the stack. (Long `a || b || ...` chains build left-deep trees,
/// hence the generous limit.)
pub(crate) const MAX_DEPTH: usize = 256;

/// Maximum nesting of parentheses, function arguments, index brackets and
/// `!` operators, which bounds recursion in the parser itself. GitHub's limit
/// is also 50.
pub(crate) const MAX_NESTING: usize = 50;

/// An expression together with the depth of its tree.
type Node = (Expr, usize);

pub(crate) fn parse(src: &str) -> Result<Expr, Error> {
    let tokens = tokenize(src)?;
    if tokens.is_empty() {
        return Err(Error::parse_at("Expected an expression", 0, src));
    }
    let mut parser = Parser {
        src,
        tokens: tokens.into_iter().peekable(),
        nesting: 0,
    };
    let (expr, _) = parser.parse_binary(0)?;
    match parser.tokens.next() {
        Some(token) => Err(parser.unexpected(&token)),
        None => Ok(expr),
    }
}

#[derive(Clone, Copy)]
enum BinaryOp {
    Or,
    And,
    Compare(CmpOp),
}

impl BinaryOp {
    /// The operator for a token and its precedence (higher binds tighter).
    fn from_token(kind: &TokenKind) -> Option<(Self, u8)> {
        Some(match kind {
            TokenKind::Or => (BinaryOp::Or, 1),
            TokenKind::And => (BinaryOp::And, 2),
            TokenKind::Eq => (BinaryOp::Compare(CmpOp::Eq), 3),
            TokenKind::Ne => (BinaryOp::Compare(CmpOp::Ne), 3),
            TokenKind::Lt => (BinaryOp::Compare(CmpOp::Lt), 4),
            TokenKind::Le => (BinaryOp::Compare(CmpOp::Le), 4),
            TokenKind::Gt => (BinaryOp::Compare(CmpOp::Gt), 4),
            TokenKind::Ge => (BinaryOp::Compare(CmpOp::Ge), 4),
            _ => return None,
        })
    }

    fn build(self, left: Expr, right: Expr) -> Expr {
        let (left, right) = (Box::new(left), Box::new(right));
        match self {
            BinaryOp::Or => Expr::Or(left, right),
            BinaryOp::And => Expr::And(left, right),
            BinaryOp::Compare(op) => Expr::Compare(left, op, right),
        }
    }
}

struct Parser<'s> {
    src: &'s str,
    tokens: Peekable<IntoIter<Token<'s>>>,
    nesting: usize,
}

impl<'s> Parser<'s> {
    fn unexpected(&self, token: &Token<'_>) -> Error {
        Error::parse_at(
            format!("Unexpected symbol: '{}'", token.text),
            token.pos,
            self.src,
        )
    }

    fn end_of_input(&self, expected: &str) -> Error {
        Error::parse_at(
            format!("Unexpected end of expression, expected {expected}"),
            self.src.len(),
            self.src,
        )
    }

    /// Consumes the next token if it has the given kind.
    fn eat(&mut self, kind: &TokenKind) -> bool {
        self.tokens.next_if(|t| t.kind == *kind).is_some()
    }

    fn next_token(&mut self, expected: &str) -> Result<Token<'s>, Error> {
        self.tokens
            .next()
            .ok_or_else(|| self.end_of_input(expected))
    }

    fn expect(&mut self, kind: &TokenKind, expected: &str) -> Result<(), Error> {
        let token = self.next_token(expected)?;
        if token.kind == *kind {
            Ok(())
        } else {
            Err(Error::parse_at(
                format!("Unexpected symbol: '{}', expected {expected}", token.text),
                token.pos,
                self.src,
            ))
        }
    }

    fn peek_pos(&mut self) -> usize {
        self.tokens.peek().map_or(self.src.len(), |t| t.pos)
    }

    /// Enters one level of nesting; pair with `self.nesting -= 1`.
    fn enter(&mut self, pos: usize) -> Result<(), Error> {
        if self.nesting >= MAX_NESTING {
            return Err(Error::parse_at(
                format!("Exceeded max expression nesting {MAX_NESTING}"),
                pos,
                self.src,
            ));
        }
        self.nesting += 1;
        Ok(())
    }

    /// Parses a nested full expression (in parentheses, brackets or arguments).
    fn parse_nested(&mut self, pos: usize) -> Result<Node, Error> {
        self.enter(pos)?;
        let node = self.parse_binary(0);
        self.nesting -= 1;
        node
    }

    /// The depth of a node built from children of the given depth.
    fn node_depth(&self, child_depth: usize, pos: usize) -> Result<usize, Error> {
        if child_depth >= MAX_DEPTH {
            return Err(Error::parse_at(
                format!("Exceeded max expression depth {MAX_DEPTH}"),
                pos,
                self.src,
            ));
        }
        Ok(child_depth + 1)
    }

    /// Parses operands joined by binary operators of precedence `min_prec`
    /// or tighter.
    fn parse_binary(&mut self, min_prec: u8) -> Result<Node, Error> {
        let (mut left, mut depth) = self.parse_unary()?;
        while let Some((op, prec, pos)) = self.tokens.peek().and_then(|t| {
            let (op, prec) = BinaryOp::from_token(&t.kind)?;
            (prec >= min_prec).then_some((op, prec, t.pos))
        }) {
            self.tokens.next();
            let (right, right_depth) = self.parse_binary(prec + 1)?;
            depth = self.node_depth(depth.max(right_depth), pos)?;
            left = op.build(left, right);
        }
        Ok((left, depth))
    }

    fn parse_unary(&mut self) -> Result<Node, Error> {
        let pos = self.peek_pos();
        if !self.eat(&TokenKind::Not) {
            return self.parse_postfix();
        }
        self.enter(pos)?;
        let operand = self.parse_unary();
        self.nesting -= 1;
        let (operand, depth) = operand?;
        Ok((Expr::Not(Box::new(operand)), self.node_depth(depth, pos)?))
    }

    /// A primary expression followed by any number of `.name`, `.*`, `[expr]`
    /// and `[*]` accessors. Like GitHub, only named values, function calls and
    /// parenthesized expressions may be dereferenced, not literals.
    ///
    /// The work is split into helpers to keep the frames on the recursive path
    /// small in debug builds.
    fn parse_postfix(&mut self) -> Result<Node, Error> {
        let token = self.next_token("an expression")?;
        let primary = match token.kind {
            TokenKind::LParen => self.parse_group(token.pos)?,
            TokenKind::Ident => match literal_keyword(token.text) {
                Some(literal) => return Ok((literal, 1)),
                None => self.parse_named(&token)?,
            },
            TokenKind::Number(n) => return Ok((Expr::Number(n), 1)),
            TokenKind::String(s) => return Ok((Expr::String(s), 1)),
            _ => return Err(self.unexpected(&token)),
        };
        self.parse_accessors(primary)
    }

    fn parse_group(&mut self, pos: usize) -> Result<Node, Error> {
        let inner = self.parse_nested(pos)?;
        self.expect(&TokenKind::RParen, "')'")?;
        Ok(inner)
    }

    /// A function call or a named value (context).
    fn parse_named(&mut self, token: &Token<'s>) -> Result<Node, Error> {
        if self
            .tokens
            .peek()
            .is_some_and(|t| t.kind == TokenKind::LParen)
        {
            self.parse_call(token.text, token.pos)
        } else {
            Ok((Expr::Context(token.text.to_ascii_lowercase()), 1))
        }
    }

    fn parse_accessors(&mut self, (mut expr, mut depth): Node) -> Result<Node, Error> {
        loop {
            let pos = self.peek_pos();
            if self.eat(&TokenKind::Dot) {
                let token = self.next_token("a property name or '*'")?;
                depth = self.node_depth(depth, pos)?;
                expr = match token.kind {
                    TokenKind::Ident => Expr::Index(
                        Box::new(expr),
                        Box::new(Expr::String(token.text.to_owned())),
                    ),
                    TokenKind::Star => Expr::Star(Box::new(expr)),
                    _ => return Err(self.unexpected(&token)),
                };
            } else if self.eat(&TokenKind::LBracket) {
                if self.eat(&TokenKind::Star) {
                    self.expect(&TokenKind::RBracket, "']'")?;
                    depth = self.node_depth(depth, pos)?;
                    expr = Expr::Star(Box::new(expr));
                } else {
                    let (key, key_depth) = self.parse_nested(pos)?;
                    self.expect(&TokenKind::RBracket, "']'")?;
                    depth = self.node_depth(depth.max(key_depth), pos)?;
                    expr = Expr::Index(Box::new(expr), Box::new(key));
                }
            } else {
                return Ok((expr, depth));
            }
        }
    }

    fn parse_call(&mut self, name: &str, pos: usize) -> Result<Node, Error> {
        self.expect(&TokenKind::LParen, "'('")?;
        let mut args = Vec::new();
        let mut depth = 0;
        if !self.eat(&TokenKind::RParen) {
            loop {
                let (arg, arg_depth) = self.parse_nested(pos)?;
                args.push(arg);
                depth = depth.max(arg_depth);
                if !self.eat(&TokenKind::Comma) {
                    self.expect(&TokenKind::RParen, "',' or ')'")?;
                    break;
                }
            }
        }
        let depth = self.node_depth(depth, pos)?;
        Ok((Expr::Call(name.to_ascii_lowercase(), args), depth))
    }
}

/// The literal a keyword stands for, unless it is a property name (after `.`).
/// These keywords are case-sensitive.
fn literal_keyword(text: &str) -> Option<Expr> {
    Some(match text {
        "null" => Expr::Null,
        "true" => Expr::Bool(true),
        "false" => Expr::Bool(false),
        "NaN" => Expr::Number(f64::NAN),
        "Infinity" => Expr::Number(f64::INFINITY),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(name: &str) -> Box<Expr> {
        Box::new(Expr::Context(name.into()))
    }

    fn string(s: &str) -> Box<Expr> {
        Box::new(Expr::String(s.into()))
    }

    #[test]
    fn property_access_keeps_case_and_context_is_lowercased() {
        assert_eq!(
            parse("GitHub.Event_Name").unwrap(),
            Expr::Index(ctx("github"), string("Event_Name"))
        );
        assert_eq!(
            parse("secrets['A']").unwrap(),
            Expr::Index(ctx("secrets"), string("A"))
        );
    }

    #[test]
    fn keywords_are_property_names_after_a_dot() {
        assert_eq!(
            parse("x.null").unwrap(),
            Expr::Index(ctx("x"), string("null"))
        );
        assert_eq!(parse("x.true.NaN").unwrap(), {
            Expr::Index(
                Box::new(Expr::Index(ctx("x"), string("true"))),
                string("NaN"),
            )
        });
    }

    #[test]
    fn stars() {
        assert_eq!(
            parse("a.*.b").unwrap(),
            Expr::Index(Box::new(Expr::Star(ctx("a"))), string("b"))
        );
        assert_eq!(parse("a[*]").unwrap(), Expr::Star(ctx("a")));
        assert!(parse("*").is_err());
        assert!(parse("contains(*, 1)").is_err());
    }

    #[test]
    fn function_names_are_lowercased() {
        assert_eq!(
            parse("toJSON(x)").unwrap(),
            Expr::Call("tojson".into(), vec![Expr::Context("x".into())])
        );
        assert_eq!(
            parse("always()").unwrap(),
            Expr::Call("always".into(), vec![])
        );
        assert_eq!(
            parse("f (1, 'a')").unwrap(),
            Expr::Call(
                "f".into(),
                vec![Expr::Number(1.0), Expr::String("a".into())]
            )
        );
    }

    #[test]
    fn precedence() {
        // `!` binds tighter than `==`, which is looser than `<`.
        assert_eq!(
            parse("!a == b < c").unwrap(),
            Expr::Compare(
                Box::new(Expr::Not(ctx("a"))),
                CmpOp::Eq,
                Box::new(Expr::Compare(ctx("b"), CmpOp::Lt, ctx("c"))),
            )
        );
        assert_eq!(
            parse("a || b && c").unwrap(),
            Expr::Or(ctx("a"), Box::new(Expr::And(ctx("b"), ctx("c"))))
        );
        assert_eq!(
            parse("a && b && c").unwrap(),
            Expr::And(Box::new(Expr::And(ctx("a"), ctx("b"))), ctx("c"))
        );
    }

    #[test]
    fn literals_cannot_be_dereferenced() {
        for src in ["'abc'.length", "'abc'[0]", "null.x", "1[0]", "true()"] {
            assert!(parse(src).is_err(), "{src}");
        }
        // But groups and function results can be.
        assert!(parse("(a).b").is_ok());
        assert!(parse("fromJSON('{}').b[0]").is_ok());
    }

    #[test]
    fn syntax_errors() {
        for src in [
            "", "   ", "a b", "(a", "a)", "a.", "a[", "a[1", "f(", "f(1,)", "f(,1)", "a ==", "!",
            "a.1", "&& a", "a[]", "a.(b)",
        ] {
            assert!(parse(src).is_err(), "{src:?} should not parse");
        }
    }

    #[test]
    fn depth_limit() {
        let deep_parens = format!("{}1{}", "(".repeat(10_000), ")".repeat(10_000));
        assert!(parse(&deep_parens).is_err());
        let many_nots = format!("{}true", "!".repeat(10_000));
        assert!(parse(&many_nots).is_err());
        let long_chain = vec!["a"; 10_000].join(" || ");
        assert!(parse(&long_chain).is_err());
        let long_path = format!("a{}", ".b".repeat(10_000));
        assert!(parse(&long_path).is_err());
        let nested_calls = format!("{}1{}", "f(".repeat(10_000), ")".repeat(10_000));
        assert!(parse(&nested_calls).is_err());

        let ok_chain = vec!["a"; MAX_DEPTH].join(" || ");
        assert!(parse(&ok_chain).is_ok());
        let ok_parens = format!("{}1{}", "(".repeat(MAX_NESTING), ")".repeat(MAX_NESTING));
        assert!(parse(&ok_parens).is_ok());
    }
}
