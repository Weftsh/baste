//! Parser and evaluator for GitHub Actions expressions (`${{ ... }}`).
//!
//! The semantics follow the GitHub Actions runner:
//!
//! - loose, JavaScript-like equality where strings compare case-insensitively
//!   and mismatched primitive types are compared as numbers;
//! - `&&` and `||` short-circuit and return one of their operands;
//! - property access is case-insensitive and yields `null` for anything
//!   missing, and `*` filters map later property accesses over elements;
//! - the built-in functions `contains`, `startsWith`, `endsWith`, `format`,
//!   `join`, `toJSON` and `fromJSON`. Status functions (`success()`, ...)
//!   and `hashFiles` come from the host through [`Functions`].
//!
//! Evaluation works on `f64` numbers, like the runner. JSON cannot hold NaN or
//! the infinities, so when one of those is part of a returned [`Value`] it is
//! represented by the string `"NaN"`, `"Infinity"` or `"-Infinity"`.
//! Integral results are returned as JSON integers.

mod analysis;
mod eval;
mod functions;
mod lexer;
mod parser;
mod template;
mod value;

pub use serde_json::{Map, Value};

pub use analysis::{contains_status_function, referenced_properties, referenced_secrets};
pub use template::{evaluate_condition, has_expression, interpolate, interpolate_value};
pub use value::{is_truthy, to_display_string};

/// A parsed expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    /// Top-level named value, stored lowercase: `github`, `env`, `matrix`, ...
    Context(String),
    /// `a.b` or `a['b']` or `a[0]` or `a[expr]`
    Index(Box<Expr>, Box<Expr>),
    /// `a.*` or `a[*]` object/array filter
    Star(Box<Expr>),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Compare(Box<Expr>, CmpOp, Box<Expr>),
    /// Function name stored lowercase
    Call(String, Vec<Expr>),
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// An error from parsing or evaluating an expression.
#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum Error {
    /// A syntax error. `pos` is the 0-based byte offset into `source_text`,
    /// which is the expression (or, for an unterminated `${{`, the template)
    /// being parsed.
    #[error("{message} (at position {pos} in '{source_text}')")]
    Parse {
        message: String,
        pos: usize,
        source_text: String,
    },
    #[error("{0}")]
    Eval(String),
}

impl Error {
    pub(crate) fn parse_at(message: impl Into<String>, pos: usize, source_text: &str) -> Self {
        Error::Parse {
            message: message.into(),
            pos,
            source_text: source_text.to_owned(),
        }
    }

    pub(crate) fn eval(message: impl Into<String>) -> Self {
        Error::Eval(message.into())
    }
}

/// Host-provided functions: status functions (success/failure/always/cancelled) and hashFiles.
pub trait Functions {
    /// `name` is lowercase. Return `None` if the host does not provide this function.
    fn call(&self, name: &str, args: &[Value]) -> Option<Result<Value, Error>>;
}

/// A [`Functions`] impl that provides nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFunctions;

impl Functions for NoFunctions {
    fn call(&self, _name: &str, _args: &[Value]) -> Option<Result<Value, Error>> {
        None
    }
}

/// Everything an expression can refer to.
#[derive(Clone, Copy)]
pub struct Env<'a> {
    /// Available contexts keyed by lowercase name (github, env, vars, job, jobs,
    /// steps, runner, secrets, strategy, matrix, needs, inputs).
    pub contexts: &'a Map<String, Value>,
    pub functions: &'a dyn Functions,
}

/// Parses a bare expression (without the `${{ }}` wrapper).
pub fn parse(src: &str) -> Result<Expr, Error> {
    parser::parse(src)
}

/// Evaluates a parsed expression.
///
/// Every context the expression names must exist in `env.contexts`, even in
/// branches that short-circuiting skips, matching GitHub's up-front check.
pub fn evaluate(expr: &Expr, env: &Env) -> Result<Value, Error> {
    Ok(eval::eval_checked(expr, env)?.into_value())
}

/// Parses and evaluates a bare expression (no `${{ }}`).
pub fn eval_str(src: &str, env: &Env) -> Result<Value, Error> {
    evaluate(&parse(src)?, env)
}
