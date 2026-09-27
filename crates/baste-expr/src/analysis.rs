//! Static analysis of parsed expressions.

use crate::template::for_each_expression;
use crate::Expr;

const STATUS_FUNCTIONS: [&str; 4] = ["success", "failure", "always", "cancelled"];

/// Calls `f` on `expr` and every sub-expression, in source order.
pub(crate) fn walk<'e>(expr: &'e Expr, f: &mut impl FnMut(&'e Expr)) {
    f(expr);
    match expr {
        Expr::Index(a, b) | Expr::And(a, b) | Expr::Or(a, b) | Expr::Compare(a, _, b) => {
            walk(a, f);
            walk(b, f);
        }
        Expr::Star(a) | Expr::Not(a) => walk(a, f),
        Expr::Call(_, args) => args.iter().for_each(|arg| walk(arg, f)),
        Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::String(_) | Expr::Context(_) => {}
    }
}

/// Whether `success()`, `failure()`, `always()` or `cancelled()` is called
/// anywhere in `expr`.
pub fn contains_status_function(expr: &Expr) -> bool {
    let mut found = false;
    walk(expr, &mut |e| {
        if let Expr::Call(name, _) = e {
            found |= STATUS_FUNCTIONS
                .iter()
                .any(|s| name.eq_ignore_ascii_case(s));
        }
    });
    found
}

/// Every secret name referenced in the `${{ }}` blocks of `text`, as
/// `secrets.NAME` or `secrets['NAME']`, anywhere in the expressions.
///
/// With `bare`, `text` is a bare expression (as in `if:`), unless it contains
/// `${{`, in which case its blocks are scanned just as GitHub does for `if:`.
/// Names are returned as written, deduplicated, in first-seen order.
/// Expressions that fail to parse are skipped.
pub fn referenced_secrets(text: &str, bare: bool) -> Vec<String> {
    referenced_properties(text, bare)
        .into_iter()
        .filter(|(context, _)| context == "secrets")
        .map(|(_, name)| name)
        .collect()
}

/// Like [`referenced_secrets`] but for every context: returns
/// `(context_lowercase, first property name)` pairs, e.g. `("needs", "build")`
/// for `needs.build.outputs.x` or `("steps", "test")` for
/// `steps['test'].outcome`. Only literal property names are reported.
pub fn referenced_properties(text: &str, bare: bool) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    for_each_expression(text, bare, |expr| {
        walk(expr, &mut |e| {
            if let Expr::Index(base, key) = e {
                if let (Expr::Context(context), Expr::String(name)) = (&**base, &**key) {
                    if !found.iter().any(|(c, n)| c == context && n == name) {
                        found.push((context.to_ascii_lowercase(), name.clone()));
                    }
                }
            }
        });
    });
    found
}
