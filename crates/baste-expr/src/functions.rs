//! Built-in functions, and dispatch of everything else to the host.

use std::borrow::Cow;

use serde_json::Value;

use crate::eval::eval;
use crate::value::{fold_case, loose_eq, Val};
use crate::{Env, Error, Expr};

pub(crate) fn call<'a>(name: &str, args: &'a [Expr], env: &Env<'a>) -> Result<Val<'a>, Error> {
    match name {
        "contains" => {
            check_arity("contains", args, 2, 2)?;
            contains(args, env)
        }
        "startswith" => {
            check_arity("startsWith", args, 2, 2)?;
            affix(args, env, |s, prefix| s.starts_with(prefix))
        }
        "endswith" => {
            check_arity("endsWith", args, 2, 2)?;
            affix(args, env, |s, suffix| s.ends_with(suffix))
        }
        "format" => {
            check_arity("format", args, 1, usize::MAX)?;
            format(args, env)
        }
        "join" => {
            check_arity("join", args, 1, 2)?;
            join(args, env)
        }
        "tojson" => {
            check_arity("toJSON", args, 1, 1)?;
            to_json(eval(&args[0], env)?)
        }
        "fromjson" => {
            check_arity("fromJSON", args, 1, 1)?;
            from_json(&eval(&args[0], env)?)
        }
        _ => call_host(name, args, env),
    }
}

fn check_arity(name: &str, args: &[Expr], min: usize, max: usize) -> Result<(), Error> {
    if args.len() < min {
        Err(Error::eval(format!(
            "Too few parameters supplied: '{name}'"
        )))
    } else if args.len() > max {
        Err(Error::eval(format!(
            "Too many parameters supplied: '{name}'"
        )))
    } else {
        Ok(())
    }
}

fn call_host<'a>(name: &str, args: &'a [Expr], env: &Env<'a>) -> Result<Val<'a>, Error> {
    let values = args
        .iter()
        .map(|arg| eval(arg, env).map(Val::into_value))
        .collect::<Result<Vec<_>, _>>()?;
    match env.functions.call(name, &values) {
        Some(result) => result.map(Val::from_owned),
        None => Err(Error::eval(format!("Unrecognized function: '{name}'"))),
    }
}

/// `contains(search, item)`: for an array, whether any element loosely equals
/// `item`; for primitives, a case-insensitive substring test. Anything else
/// (objects, or a non-primitive `item`) is false.
fn contains<'a>(args: &'a [Expr], env: &Env<'a>) -> Result<Val<'a>, Error> {
    let search = eval(&args[0], env)?;
    if search.is_primitive() {
        let item = eval(&args[1], env)?;
        let found = item.is_primitive()
            && fold_case(&search.display()).contains(fold_case(&item.display()).as_ref());
        return Ok(Val::Bool(found));
    }
    if let Some(elements) = search.elements().filter(|e| !e.is_empty()) {
        let item = eval(&args[1], env)?;
        return Ok(Val::Bool(elements.iter().any(|e| loose_eq(&item, e))));
    }
    Ok(Val::Bool(false))
}

/// `startsWith`/`endsWith`: case-insensitive, both arguments must be primitives.
fn affix<'a>(
    args: &'a [Expr],
    env: &Env<'a>,
    test: fn(&str, &str) -> bool,
) -> Result<Val<'a>, Error> {
    let subject = eval(&args[0], env)?;
    if !subject.is_primitive() {
        return Ok(Val::Bool(false));
    }
    let affix = eval(&args[1], env)?;
    let matched =
        affix.is_primitive() && test(&fold_case(&subject.display()), &fold_case(&affix.display()));
    Ok(Val::Bool(matched))
}

/// `format(fmt, args...)`: replaces `{N}` with the display string of argument
/// `N`; `{{` and `}}` are literal braces. Arguments are evaluated on first use.
fn format<'a>(args: &'a [Expr], env: &Env<'a>) -> Result<Val<'a>, Error> {
    let format_val = eval(&args[0], env)?;
    let fmt = format_val.display();
    let fmt = fmt.as_ref();
    let bytes = fmt.as_bytes();
    let invalid = || Error::eval(format!("The following format string is invalid: '{fmt}'"));

    // (type name, display string) of each argument evaluated so far.
    let mut cache: Vec<Option<(&str, String)>> = vec![None; args.len() - 1];
    let mut out = String::with_capacity(fmt.len());
    let mut i = 0;
    while i < fmt.len() {
        let left = fmt[i..].find('{').map(|p| p + i);
        let right = fmt[i..].find('}').map(|p| p + i);
        match (left, right) {
            (Some(l), r) if r.is_none_or(|r| r > l) => {
                if bytes.get(l + 1) == Some(&b'{') {
                    out.push_str(&fmt[i..=l]);
                    i = l + 2;
                    continue;
                }
                let (arg, specifiers, close) = read_placeholder(fmt, l).ok_or_else(invalid)?;
                if arg >= cache.len() {
                    return Err(Error::eval(format!(
                        "The following format string references more arguments than were supplied: '{fmt}'"
                    )));
                }
                out.push_str(&fmt[i..l]);
                let (kind, text) = match &mut cache[arg] {
                    Some(cached) => cached,
                    slot => {
                        let value = eval(&args[arg + 1], env)?;
                        slot.insert((value.kind_name(), value.display().into_owned()))
                    }
                };
                // The runner only supports specifiers for dates, which
                // expressions cannot produce.
                if !specifiers.is_empty() {
                    return Err(Error::eval(format!(
                        "The format specifiers '{specifiers}' are not valid for objects of type '{kind}'"
                    )));
                }
                out.push_str(text);
                i = close + 1;
            }
            (_, Some(r)) => {
                if bytes.get(r + 1) != Some(&b'}') {
                    return Err(invalid());
                }
                out.push_str(&fmt[i..=r]);
                i = r + 2;
            }
            _ => {
                out.push_str(&fmt[i..]);
                break;
            }
        }
    }
    Ok(Val::String(Cow::Owned(out)))
}

/// Reads `{N}` or `{N:specifiers}` starting at the `{` at `open`. Returns the
/// argument index, the specifiers (with `}}` unescaped) and the offset of the
/// closing `}`.
fn read_placeholder(fmt: &str, open: usize) -> Option<(usize, String, usize)> {
    let bytes = fmt.as_bytes();
    let digits_start = open + 1;
    let digits_end = digits_start
        + bytes[digits_start..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
    // Like the runner, an index must fit in a byte.
    let arg: u8 = fmt[digits_start..digits_end].parse().ok()?;
    match bytes.get(digits_end)? {
        b'}' => Some((usize::from(arg), String::new(), digits_end)),
        b':' => {
            let mut specifiers = String::new();
            let mut j = digits_end + 1;
            loop {
                let c = fmt[j..].chars().next()?;
                if c != '}' {
                    specifiers.push(c);
                    j += c.len_utf8();
                } else if bytes.get(j + 1) == Some(&b'}') {
                    specifiers.push('}');
                    j += 2;
                } else {
                    return Some((usize::from(arg), specifiers, j));
                }
            }
        }
        _ => None,
    }
}

/// `join(items, separator = ',')`: joins an array's elements as display
/// strings. A primitive is returned as a string; objects give `''`.
fn join<'a>(args: &'a [Expr], env: &Env<'a>) -> Result<Val<'a>, Error> {
    let items = eval(&args[0], env)?;
    if let Some(elements) = items.elements() {
        let separator = match args.get(1) {
            Some(arg) if elements.len() > 1 => {
                let separator = eval(arg, env)?;
                if separator.is_primitive() {
                    separator.display().into_owned()
                } else {
                    ",".to_owned()
                }
            }
            _ => ",".to_owned(),
        };
        let parts: Vec<_> = elements.iter().map(Val::display).collect();
        return Ok(Val::String(Cow::Owned(parts.join(&separator))));
    }
    if items.is_primitive() {
        return Ok(Val::String(Cow::Owned(items.display().into_owned())));
    }
    Ok(Val::String(Cow::Borrowed("")))
}

/// `toJSON(value)`: pretty-printed JSON with two-space indentation.
fn to_json(value: Val<'_>) -> Result<Val<'_>, Error> {
    let json = match &value {
        Val::Collection(c) => serde_json::to_string_pretty(c.as_ref()),
        _ => serde_json::to_string_pretty(&value.into_value()),
    };
    json.map(|s| Val::String(Cow::Owned(s)))
        .map_err(|e| Error::eval(format!("toJSON failed: {e}")))
}

/// `fromJSON(text)`: parses JSON text.
fn from_json<'a>(text: &Val<'_>) -> Result<Val<'a>, Error> {
    let text = text.display();
    serde_json::from_str::<Value>(&text)
        .map(Val::from_owned)
        .map_err(|e| Error::eval(format!("Error parsing fromJSON input '{text}': {e}")))
}
