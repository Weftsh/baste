//! Tree-walking evaluator.

use std::borrow::Cow;

use serde_json::{Map, Value};

use crate::analysis::walk;
use crate::functions;
use crate::value::{compare, eq_ignore_case, Val};
use crate::{CmpOp, Env, Error, Expr};

/// Checks that every named value exists, then evaluates. GitHub validates
/// named values when parsing, so an unknown one is an error even in a branch
/// that would be short-circuited.
pub(crate) fn eval_checked<'a>(expr: &'a Expr, env: &Env<'a>) -> Result<Val<'a>, Error> {
    let mut unknown = None;
    walk(expr, &mut |e| {
        if let Expr::Context(name) = e {
            if unknown.is_none() && lookup_context(env, name).is_none() {
                unknown = Some(name);
            }
        }
    });
    if let Some(name) = unknown {
        return Err(Error::eval(format!("Unrecognized named-value: '{name}'")));
    }
    eval(expr, env)
}

fn lookup_context<'a>(env: &Env<'a>, name: &str) -> Option<&'a Value> {
    let contexts: &'a Map<String, Value> = env.contexts;
    object_get(contexts, name)
}

// Each compound case is evaluated in its own function: debug builds give a
// function one stack slot per temporary across all match arms, so keeping
// `eval` small keeps deep (but legal) expressions from exhausting the stack.
pub(crate) fn eval<'a>(expr: &'a Expr, env: &Env<'a>) -> Result<Val<'a>, Error> {
    match expr {
        Expr::Null => Ok(Val::Null),
        Expr::Bool(b) => Ok(Val::Bool(*b)),
        Expr::Number(n) => Ok(Val::Number(*n)),
        Expr::String(s) => Ok(Val::String(Cow::Borrowed(s))),
        Expr::Context(name) => eval_context(name, env),
        Expr::Index(base, key) => eval_index(base, key, env),
        Expr::Star(base) => eval_star(base, env),
        Expr::Not(operand) => eval_not(operand, env),
        Expr::And(left, right) => eval_and(left, right, env),
        Expr::Or(left, right) => eval_or(left, right, env),
        Expr::Compare(left, op, right) => eval_compare(left, *op, right, env),
        Expr::Call(name, args) => functions::call(name, args, env),
    }
}

fn eval_context<'a>(name: &str, env: &Env<'a>) -> Result<Val<'a>, Error> {
    lookup_context(env, name)
        .map(Val::from_ref)
        .ok_or_else(|| Error::eval(format!("Unrecognized named-value: '{name}'")))
}

fn eval_index<'a>(base: &'a Expr, key: &'a Expr, env: &Env<'a>) -> Result<Val<'a>, Error> {
    let base = eval(base, env)?;
    Ok(index(base, &eval(key, env)?))
}

fn eval_star<'a>(base: &'a Expr, env: &Env<'a>) -> Result<Val<'a>, Error> {
    Ok(star(eval(base, env)?))
}

fn eval_not<'a>(operand: &'a Expr, env: &Env<'a>) -> Result<Val<'a>, Error> {
    Ok(Val::Bool(!eval(operand, env)?.is_truthy()))
}

/// `&&` returns the first falsy operand, or else the last one.
fn eval_and<'a>(left: &'a Expr, right: &'a Expr, env: &Env<'a>) -> Result<Val<'a>, Error> {
    let left = eval(left, env)?;
    if left.is_truthy() {
        eval(right, env)
    } else {
        Ok(left)
    }
}

/// `||` returns the first truthy operand, or else the last one.
fn eval_or<'a>(left: &'a Expr, right: &'a Expr, env: &Env<'a>) -> Result<Val<'a>, Error> {
    let left = eval(left, env)?;
    if left.is_truthy() {
        Ok(left)
    } else {
        eval(right, env)
    }
}

fn eval_compare<'a>(
    left: &'a Expr,
    op: CmpOp,
    right: &'a Expr,
    env: &Env<'a>,
) -> Result<Val<'a>, Error> {
    let left = eval(left, env)?;
    Ok(Val::Bool(compare(&left, op, &eval(right, env)?)))
}

/// Looks up `key` in an object, preferring an exact match and falling back
/// to a case-insensitive one.
fn object_get<'m>(map: &'m Map<String, Value>, key: &str) -> Option<&'m Value> {
    map.get(key).or_else(|| {
        map.iter()
            .find(|(k, _)| eq_ignore_case(k, key))
            .map(|(_, v)| v)
    })
}

/// Converts an index to an array position: the key is coerced to a number and
/// floored; negative, NaN and out-of-range keys match nothing.
fn array_position(key: &Val<'_>) -> Option<usize> {
    let n = key.to_number();
    if n.is_nan() || n < 0.0 || n.floor() > f64::from(i32::MAX) {
        return None;
    }
    Some(n.floor() as usize)
}

/// Indexes one JSON collection. Objects are indexed by the key's string form
/// (primitive keys only), arrays by its numeric form.
fn index_json<'v>(collection: &'v Value, key: &Val<'_>) -> Option<&'v Value> {
    match collection {
        Value::Object(map) if key.is_primitive() => object_get(map, &key.display()),
        Value::Array(items) => items.get(array_position(key)?),
        _ => None,
    }
}

fn index_collection<'a>(collection: Cow<'a, Value>, key: &Val<'_>) -> Option<Val<'a>> {
    match collection {
        Cow::Borrowed(v) => index_json(v, key).map(Val::from_ref),
        Cow::Owned(v) => index_json(&v, key).cloned().map(Val::from_owned),
    }
}

/// `base[key]`. Missing keys and indexing into non-collections give `null`.
/// On a filtered array the access is applied to every element, keeping the
/// elements that have the key/index (non-collections are dropped).
fn index<'a>(base: Val<'a>, key: &Val<'_>) -> Val<'a> {
    match base {
        Val::Collection(c) => index_collection(c, key).unwrap_or(Val::Null),
        Val::Filtered(items) => Val::Filtered(
            items
                .into_iter()
                .filter_map(|item| match item {
                    Val::Collection(c) => index_collection(c, key),
                    _ => None,
                })
                .collect(),
        ),
        _ => Val::Null,
    }
}

/// The values of an object or the elements of an array.
fn children(collection: Cow<'_, Value>) -> Vec<Val<'_>> {
    match collection {
        Cow::Borrowed(Value::Array(items)) => items.iter().map(Val::from_ref).collect(),
        Cow::Borrowed(Value::Object(map)) => map.values().map(Val::from_ref).collect(),
        Cow::Owned(Value::Array(items)) => items.into_iter().map(Val::from_owned).collect(),
        Cow::Owned(Value::Object(map)) => {
            map.into_iter().map(|(_, v)| Val::from_owned(v)).collect()
        }
        _ => Vec::new(),
    }
}

/// `base.*`: the object's values or the array's elements as a filtered
/// array. On a filtered array, the children of every element are flattened
/// into one filtered array. Anything else gives an empty filtered array.
fn star(base: Val<'_>) -> Val<'_> {
    match base {
        Val::Collection(c) => Val::Filtered(children(c)),
        Val::Filtered(items) => Val::Filtered(
            items
                .into_iter()
                .flat_map(|item| match item {
                    Val::Collection(c) => children(c),
                    _ => Vec::new(),
                })
                .collect(),
        ),
        _ => Val::Filtered(Vec::new()),
    }
}
