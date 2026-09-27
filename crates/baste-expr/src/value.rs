//! Runtime values, type coercion, comparison and string conversion.

use std::borrow::Cow;
use std::cmp::Ordering;

use serde_json::{Number, Value};

use crate::CmpOp;

/// A value produced during evaluation.
///
/// Values taken from the contexts are borrowed rather than cloned. Numbers
/// are `f64`, as in the runner, and may be NaN or infinite; they only become
/// JSON when a final result is produced.
#[derive(Debug, Clone)]
pub(crate) enum Val<'a> {
    Null,
    Bool(bool),
    Number(f64),
    String(Cow<'a, str>),
    /// A JSON array or object (never a scalar).
    Collection(Cow<'a, Value>),
    /// The result of a `*` filter. Property and index access map over its
    /// elements instead of indexing the array itself.
    Filtered(Vec<Val<'a>>),
}

impl<'a> Val<'a> {
    pub(crate) fn from_ref(v: &'a Value) -> Self {
        match v {
            Value::Null => Val::Null,
            Value::Bool(b) => Val::Bool(*b),
            Value::Number(n) => Val::Number(json_number_to_f64(n)),
            Value::String(s) => Val::String(Cow::Borrowed(s)),
            Value::Array(_) | Value::Object(_) => Val::Collection(Cow::Borrowed(v)),
        }
    }

    pub(crate) fn from_owned(v: Value) -> Self {
        match v {
            Value::Null => Val::Null,
            Value::Bool(b) => Val::Bool(b),
            Value::Number(n) => Val::Number(json_number_to_f64(&n)),
            Value::String(s) => Val::String(Cow::Owned(s)),
            v @ (Value::Array(_) | Value::Object(_)) => Val::Collection(Cow::Owned(v)),
        }
    }

    /// A borrowed view of this value.
    pub(crate) fn reborrow(&self) -> Val<'_> {
        match self {
            Val::Null => Val::Null,
            Val::Bool(b) => Val::Bool(*b),
            Val::Number(n) => Val::Number(*n),
            Val::String(s) => Val::String(Cow::Borrowed(s)),
            Val::Collection(c) => Val::Collection(Cow::Borrowed(c)),
            Val::Filtered(items) => Val::Filtered(items.iter().map(Val::reborrow).collect()),
        }
    }

    pub(crate) fn is_primitive(&self) -> bool {
        matches!(
            self,
            Val::Null | Val::Bool(_) | Val::Number(_) | Val::String(_)
        )
    }

    /// Falsy values are `false`, `0`, `-0`, `NaN`, `''` and `null`.
    pub(crate) fn is_truthy(&self) -> bool {
        match self {
            Val::Null => false,
            Val::Bool(b) => *b,
            Val::Number(n) => *n != 0.0 && !n.is_nan(),
            Val::String(s) => !s.is_empty(),
            Val::Collection(_) | Val::Filtered(_) => true,
        }
    }

    /// Numeric coercion: `null` is 0, booleans are 1/0, strings are parsed
    /// with [`parse_number`] and collections are NaN.
    pub(crate) fn to_number(&self) -> f64 {
        match self {
            Val::Null => 0.0,
            Val::Bool(b) => f64::from(u8::from(*b)),
            Val::Number(n) => *n,
            Val::String(s) => parse_number(s),
            Val::Collection(_) | Val::Filtered(_) => f64::NAN,
        }
    }

    /// String coercion, as used by `format` and `${{ }}` interpolation.
    pub(crate) fn display(&self) -> Cow<'_, str> {
        match self {
            Val::Null => Cow::Borrowed(""),
            Val::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
            Val::Number(n) => Cow::Owned(format_number(*n)),
            Val::String(s) => Cow::Borrowed(s),
            Val::Collection(c) if c.is_object() => Cow::Borrowed("Object"),
            Val::Collection(_) | Val::Filtered(_) => Cow::Borrowed("Array"),
        }
    }

    /// The runner's name for this value's type.
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Val::Null => "Null",
            Val::Bool(_) => "Boolean",
            Val::Number(_) => "Number",
            Val::String(_) => "String",
            Val::Collection(c) if c.is_object() => "Object",
            Val::Collection(_) | Val::Filtered(_) => "Array",
        }
    }

    /// The elements of an array or filtered array; `None` for anything else.
    pub(crate) fn elements(&self) -> Option<Vec<Val<'_>>> {
        match self {
            Val::Collection(c) => c
                .as_array()
                .map(|items| items.iter().map(Val::from_ref).collect()),
            Val::Filtered(items) => Some(items.iter().map(Val::reborrow).collect()),
            _ => None,
        }
    }

    pub(crate) fn into_value(self) -> Value {
        match self {
            Val::Null => Value::Null,
            Val::Bool(b) => Value::Bool(b),
            Val::Number(n) => number_value(n),
            Val::String(s) => Value::String(s.into_owned()),
            Val::Collection(c) => c.into_owned(),
            Val::Filtered(items) => Value::Array(items.into_iter().map(Val::into_value).collect()),
        }
    }
}

fn json_number_to_f64(n: &Number) -> f64 {
    n.as_f64().unwrap_or(f64::NAN)
}

/// Converts an evaluated number to JSON: an integer when it is integral and
/// fits in `i64`, otherwise a float. NaN and the infinities, which JSON cannot
/// represent, become the strings `"NaN"`, `"Infinity"` and `"-Infinity"`.
pub(crate) fn number_value(n: f64) -> Value {
    // i64::MIN is exactly representable; i64::MAX rounds up to 2^63.
    const MIN: f64 = i64::MIN as f64;
    const MAX: f64 = i64::MAX as f64;
    if n.fract() == 0.0 && (MIN..MAX).contains(&n) {
        return Value::from(n as i64);
    }
    match Number::from_f64(n) {
        Some(num) => Value::Number(num),
        None => Value::String(format_number(n)),
    }
}

/// GitHub's loose equality. Strings compare case-insensitively, primitives of
/// different types compare as numbers (`'1' == 1`, `null == 0`, `true == 1`),
/// NaN equals nothing, and arrays/objects are equal only to themselves (the
/// same value in the same context).
pub(crate) fn loose_eq(a: &Val<'_>, b: &Val<'_>) -> bool {
    match (a, b) {
        (Val::String(x), Val::String(y)) => eq_ignore_case(x, y),
        (Val::Collection(Cow::Borrowed(x)), Val::Collection(Cow::Borrowed(y))) => {
            std::ptr::eq(*x, *y)
        }
        _ if a.is_primitive() && b.is_primitive() => a.to_number() == b.to_number(),
        _ => false,
    }
}

/// Evaluates a comparison. Ordering uses the same coercions as [`loose_eq`];
/// comparisons involving NaN or collections are false.
pub(crate) fn compare(a: &Val<'_>, op: CmpOp, b: &Val<'_>) -> bool {
    let ordering = || match (a, b) {
        (Val::String(x), Val::String(y)) => Some(cmp_ignore_case(x, y)),
        _ if a.is_primitive() && b.is_primitive() => a.to_number().partial_cmp(&b.to_number()),
        _ => None,
    };
    match op {
        CmpOp::Eq => loose_eq(a, b),
        CmpOp::Ne => !loose_eq(a, b),
        CmpOp::Lt => ordering() == Some(Ordering::Less),
        CmpOp::Le => matches!(ordering(), Some(Ordering::Less | Ordering::Equal)),
        CmpOp::Gt => ordering() == Some(Ordering::Greater),
        CmpOp::Ge => matches!(ordering(), Some(Ordering::Greater | Ordering::Equal)),
    }
}

/// Folds a character for the runner's "ordinal ignore case" comparisons,
/// which compare upper-cased characters.
fn fold_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_uppercase();
    }
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) => u,
        // Characters like 'ß' upper-case to several characters; the runner
        // leaves them alone.
        _ => c,
    }
}

pub(crate) fn fold_case(s: &str) -> Cow<'_, str> {
    if s.chars().all(|c| fold_char(c) == c) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(s.chars().map(fold_char).collect())
    }
}

pub(crate) fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.chars().map(fold_char).eq(b.chars().map(fold_char))
}

fn cmp_ignore_case(a: &str, b: &str) -> Ordering {
    a.chars().map(fold_char).cmp(b.chars().map(fold_char))
}

/// Converts a string to a number like the runner does: surrounding whitespace
/// is ignored and the empty string is 0. Accepted forms are decimals with an
/// optional sign, fraction and exponent, `Infinity`/`NaN`, and 32-bit `0x`
/// hexadecimal or `0o` octal. Anything else is NaN.
pub(crate) fn parse_number(s: &str) -> f64 {
    let s = s.trim();
    if s.is_empty() {
        return 0.0;
    }
    if let Some(n) = parse_decimal(s) {
        return n;
    }
    let radix =
        |prefix: &str, radix: u32| s.strip_prefix(prefix).and_then(|d| parse_int32(d, radix));
    radix("0x", 16)
        .or_else(|| radix("0o", 8))
        .unwrap_or(f64::NAN)
}

/// `[+-]` then `digits[.digits]`, `digits.` or `.digits`, then an optional
/// exponent; or a signed `Infinity`/`NaN` (case-insensitive).
fn parse_decimal(s: &str) -> Option<f64> {
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    if unsigned.eq_ignore_ascii_case("infinity") {
        return Some(if s.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    if unsigned.eq_ignore_ascii_case("nan") {
        return Some(f64::NAN);
    }

    let bytes = unsigned.as_bytes();
    let digits_from = |mut i: usize| {
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        i
    };
    let int_end = digits_from(0);
    let mut end = int_end;
    let mut mantissa_digits = int_end;
    if bytes.get(end) == Some(&b'.') {
        let frac_end = digits_from(end + 1);
        mantissa_digits += frac_end - end - 1;
        end = frac_end;
    }
    if mantissa_digits == 0 {
        return None;
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let mut exp_start = end + 1;
        if matches!(bytes.get(exp_start), Some(b'+' | b'-')) {
            exp_start += 1;
        }
        end = digits_from(exp_start);
        if end == exp_start {
            return None;
        }
    }
    if end != bytes.len() {
        return None;
    }
    s.parse().ok()
}

/// Parses digits in `radix` as 32 bits reinterpreted as a signed integer (so
/// `0xffffffff` is -1), like .NET's `Int32` parsing.
fn parse_int32(digits: &str, radix: u32) -> Option<f64> {
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    let n = u32::from_str_radix(digits, radix).ok()?;
    Some(f64::from(n as i32))
}

/// Formats a number like JavaScript's `Number.prototype.toString`: integers
/// without a decimal point, other values in the shortest form that round-trips,
/// using exponent notation below 1e-6 and from 1e21.
pub(crate) fn format_number(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_owned();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if n.fract() == 0.0 && n.abs() < 1e15 {
        // Also turns -0 into "0".
        return (n as i64).to_string();
    }

    // `{:e}` gives the shortest round-trip digits, e.g. "1.2345e-7".
    let scientific = format!("{:e}", n.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("LowerExp output has an exponent");
    let digits: String = mantissa.chars().filter(|&c| c != '.').collect();
    let exponent: i32 = exponent.parse().expect("LowerExp exponent is an integer");
    // The value is 0.<digits> * 10^point.
    let point = exponent + 1;
    let len = digits.len() as i32;

    let mut out = String::new();
    if n < 0.0 {
        out.push('-');
    }
    if len <= point && point <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (point - len) as usize));
    } else if 0 < point && point <= 21 {
        let (int, frac) = digits.split_at(point as usize);
        out.push_str(int);
        out.push('.');
        out.push_str(frac);
    } else if -6 < point && point <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-point) as usize));
        out.push_str(&digits);
    } else {
        let (first, rest) = digits.split_at(1);
        out.push_str(first);
        if !rest.is_empty() {
            out.push('.');
            out.push_str(rest);
        }
        out.push('e');
        out.push(if exponent < 0 { '-' } else { '+' });
        out.push_str(&exponent.abs().to_string());
    }
    out
}

/// Falsy values are `false`, `0`, `-0`, `NaN`, `""` and `null`; everything
/// else, including empty arrays and objects, is truthy.
///
/// Note that the string `"NaN"` (how evaluation results represent NaN) is a
/// non-empty string and therefore truthy.
pub fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => {
            let n = json_number_to_f64(n);
            n != 0.0 && !n.is_nan()
        }
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Converts a value to a string the way `format` and `${{ }}` interpolation do:
/// null -> `""`, bool -> `"true"`/`"false"`, numbers without a trailing `.0`
/// (floats in JavaScript's shortest form), strings unchanged, arrays ->
/// `"Array"` and objects -> `"Object"`.
pub fn to_display_string(v: &Value) -> String {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                format_number(json_number_to_f64(n))
            }
        }
        Value::String(s) => s.clone(),
        other => Val::from_ref(other).display().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_numbers_like_javascript() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (-0.0, "0"),
            (3.0, "3"),
            (-7.0, "-7"),
            (1.5, "1.5"),
            (-2.5, "-2.5"),
            (0.1, "0.1"),
            (0.000001, "0.000001"),
            (1e-7, "1e-7"),
            (1.5e-10, "1.5e-10"),
            (123.456, "123.456"),
            (1e15, "1000000000000000"),
            (123456789012345680.0, "123456789012345680"),
            (1e21, "1e+21"),
            (1.2345e25, "1.2345e+25"),
            (-1e21, "-1e+21"),
            (9007199254740993.0, "9007199254740992"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
            (1.0 / 3.0, "0.3333333333333333"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ];
        for &(n, expected) in cases {
            assert_eq!(format_number(n), expected, "{n:?}");
        }
    }

    #[test]
    fn parses_numbers_like_the_runner() {
        let cases: &[(&str, f64)] = &[
            ("", 0.0),
            ("   ", 0.0),
            ("0", 0.0),
            (" 42 ", 42.0),
            ("-1", -1.0),
            ("+1", 1.0),
            ("1.", 1.0),
            (".5", 0.5),
            ("-.5", -0.5),
            ("1e3", 1000.0),
            ("1E+3", 1000.0),
            ("2.5e-1", 0.25),
            ("007", 7.0),
            ("0x1F", 31.0),
            ("0xffffffff", -1.0),
            ("0o17", 15.0),
            ("Infinity", f64::INFINITY),
            ("-infinity", f64::NEG_INFINITY),
        ];
        for &(s, expected) in cases {
            assert_eq!(parse_number(s), expected, "{s:?}");
        }
        for s in [
            "abc",
            "1a",
            "1e",
            "e1",
            ".",
            "-",
            "+",
            "1.2.3",
            "0x",
            "0xg",
            "0X1F",
            "-0x1",
            "0x100000000",
            "1,000",
            "inf",
            "NaN",
            "1_000",
            "0b1",
        ] {
            assert!(parse_number(s).is_nan(), "{s:?}");
        }
    }

    #[test]
    fn number_values() {
        assert_eq!(number_value(3.0), Value::from(3));
        assert_eq!(number_value(-0.0), Value::from(0));
        assert_eq!(number_value(1.5), Value::from(1.5));
        assert_eq!(number_value(1e300), Value::from(1e300));
        assert_eq!(number_value(f64::NAN), Value::from("NaN"));
        assert_eq!(number_value(f64::NEG_INFINITY), Value::from("-Infinity"));
    }

    #[test]
    fn case_folding() {
        assert!(eq_ignore_case("héllo", "HÉLLO"));
        assert!(!eq_ignore_case("a", "ab"));
        // Upper-case folding puts '_' after letters.
        assert_eq!(cmp_ignore_case("a", "_"), Ordering::Less);
        assert_eq!(fold_case("straße"), "STRAßE");
    }
}
