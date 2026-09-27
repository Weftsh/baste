mod common;

use baste_expr::{eval_str, evaluate, parse, Env, Error, Expr, NoFunctions, Value};
use common::{contexts, eval, eval_err, eval_message, parse_error, with_env, Host};
use serde_json::json;

fn assert_all(expected: &Value, sources: &[&str]) {
    for src in sources {
        assert_eq!(&eval(src), expected, "{src}");
    }
}

// ---------------------------------------------------------------- literals

#[test]
fn literals() {
    assert_eq!(eval("null"), Value::Null);
    assert_eq!(eval("true"), json!(true));
    assert_eq!(eval("false"), json!(false));
    assert_eq!(eval("711"), json!(711));
    assert_eq!(eval("-9.2"), json!(-9.2));
    assert_eq!(eval("+3"), json!(3));
    assert_eq!(eval("0x2A"), json!(42));
    assert_eq!(eval("0xff"), json!(255));
    assert_eq!(eval("-2.99e-2"), json!(-0.0299));
    assert_eq!(eval("1e3"), json!(1000));
    assert_eq!(eval(".5"), json!(0.5));
    assert_eq!(eval("'Mona the Octocat'"), json!("Mona the Octocat"));
    assert_eq!(eval("'It''s open source!'"), json!("It's open source!"));
    assert_eq!(eval("''"), json!(""));
    assert_eq!(eval("'héllo wörld ✓'"), json!("héllo wörld ✓"));
}

#[test]
fn nan_and_infinity_become_strings_in_results() {
    assert_eq!(eval("NaN"), json!("NaN"));
    assert_eq!(eval("Infinity"), json!("Infinity"));
    assert_eq!(eval("-Infinity"), json!("-Infinity"));
    // But they are still numbers during evaluation.
    assert_eq!(eval("Infinity > 1e308"), json!(true));
    assert_eq!(eval("!NaN"), json!(true));
}

#[test]
fn integral_results_are_integers() {
    let one = eval("1.0");
    assert!(one.is_i64(), "{one:?}");
    assert_eq!(one, json!(1));
    assert!(eval("-0").is_i64());
    assert!(eval("2.5").is_f64());
    assert!(eval("1e300").is_f64());
    assert_eq!(eval("github.event.size"), json!(1.5));
    assert_eq!(eval("github.run_number"), json!(42));
}

#[test]
fn double_quoted_strings_are_rejected() {
    let (message, pos, source) = parse_error("github.ref == \"main\"");
    assert_eq!(pos, 14);
    assert_eq!(source, "github.ref == \"main\"");
    assert!(message.contains("Unexpected symbol"), "{message}");
    assert!(message.contains("single quotes"), "{message}");
}

#[test]
fn literal_keywords_are_case_sensitive() {
    // `TRUE` is a named value, not a boolean.
    assert_eq!(eval_message("TRUE"), "Unrecognized named-value: 'true'");
    assert_eq!(eval_message("Null"), "Unrecognized named-value: 'null'");
}

#[test]
fn whitespace_between_tokens() {
    assert_eq!(eval("  github . event_name\t==\n'push'  "), json!(true));
    assert_eq!(eval("contains ( 'abc' , 'B' )"), json!(true));
    assert_eq!(eval("github [ 'ref' ]"), json!("refs/heads/main"));
    assert_eq!(eval("! false"), json!(true));
}

// ------------------------------------------------------------- operators

#[test]
fn precedence() {
    assert_eq!(eval("!true == false"), json!(true));
    assert_eq!(eval("!(true == false)"), json!(true));
    assert_eq!(eval("1 < 2 == true"), json!(true));
    assert_eq!(eval("true == 1 < 2"), json!(true));
    assert_eq!(eval("true || false && false"), json!(true));
    assert_eq!(eval("false && true || true"), json!(true));
    assert_eq!(eval("(true || false) && false"), json!(false));
    assert_eq!(eval("1 == 1 && 2 == 2"), json!(true));
    assert_eq!(
        eval("!github.event.forced && matrix.experimental"),
        json!(true)
    );
}

#[test]
fn logical_operators_return_operand_values() {
    assert_eq!(eval("'a' && 'b'"), json!("b"));
    assert_eq!(eval("'' && 'b'"), json!(""));
    assert_eq!(eval("0 && 'b'"), json!(0));
    assert_eq!(eval("null || 'default'"), json!("default"));
    assert_eq!(eval("0 || 5"), json!(5));
    assert_eq!(eval("'x' || 'y'"), json!("x"));
    assert_eq!(eval("'' || 0"), json!(0));
    assert_eq!(eval("github.missing || 'fallback'"), json!("fallback"));
    assert_eq!(eval("matrix || 'fallback'"), eval("matrix"));
    // The ternary idiom.
    assert_eq!(
        eval("github.ref == 'refs/heads/main' && 'production' || 'staging'"),
        json!("production")
    );
    assert_eq!(
        eval("github.ref == 'refs/heads/dev' && 'production' || 'staging'"),
        json!("staging")
    );
}

#[test]
fn logical_operators_short_circuit() {
    // The right-hand side would fail to evaluate.
    assert_eq!(eval("false && fromJSON('not json')"), json!(false));
    assert_eq!(eval("true || fromJSON('not json')"), json!(true));
    assert!(matches!(
        eval_err("true && fromJSON('not json')"),
        Error::Eval(_)
    ));
}

#[test]
fn not() {
    assert_all(
        &json!(true),
        &[
            "!0",
            "!-0",
            "!''",
            "!null",
            "!NaN",
            "!false",
            "!github.missing",
            "!env.EMPTY",
        ],
    );
    assert_all(
        &json!(false),
        &[
            "!'0'",
            "!'false'",
            "!fromJSON('[]')",
            "!fromJSON('{}')",
            "!github.missing.*",
            "!env.ZERO",
            "!1",
            "!Infinity",
        ],
    );
    assert_eq!(eval("!!'x'"), json!(true));
}

#[test]
fn loose_equality() {
    assert_all(
        &json!(true),
        &[
            "'1' == 1",
            "1 == '1'",
            "null == 0",
            "0 == null",
            "true == 1",
            "false == 0",
            "true == '1'",
            "'' == 0",
            "'' == null",
            "' 5 ' == 5",
            "'1e2' == 100",
            "'0x10' == 16",
            "'abc' == 'ABC'",
            "'Straße' == 'STRAßE'",
            "'Ünïcödé' == 'üNÏCÖDÉ'",
            "null == null",
            "1 == 1.0",
            "-0 == 0",
            "NaN != NaN",
            "NaN != 1",
            "'abc' != 0",
            "'true' != true",
            "matrix.node == '18'",
            "github.event == github.event",
            "github.event.commits[0] == github.event.commits[0]",
        ],
    );
    assert_all(
        &json!(false),
        &[
            "'abc' == 0",
            "NaN == NaN",
            "NaN == 'NaN'",
            "'true' == true",
            "null == 'null'",
            "null == false == false",
            "'a' == 'b'",
            "true == 2",
            "fromJSON('[]') == fromJSON('[]')",
            "fromJSON('{}') == fromJSON('{}')",
            "github.event == 'Object'",
            "fromJSON('[1]') == 1",
            "github.event.commits == github.event.commits.*",
            "github.event == github.event.issue",
        ],
    );
}

#[test]
fn comparisons() {
    assert_all(
        &json!(true),
        &[
            "1 < 2",
            "2 <= 2",
            "2 >= 2",
            "3 > 2",
            "-1 < 0",
            "'-1' < 0",
            "'a' < 'B'",
            "'B' > 'a'",
            "'abc' <= 'ABC'",
            "'abc' >= 'ABC'",
            "'apple' < 'banana'",
            "'10' > 9",
            "'10' < '9'",
            "null < 1",
            "null <= null",
            "null >= null",
            "true > false",
            "false < true",
            "true >= 1",
            // Upper-case folding puts '_' after the letters.
            "'a' < '_'",
            "Infinity > 1e308",
        ],
    );
    assert_all(
        &json!(false),
        &[
            "NaN < 1",
            "1 < NaN",
            "NaN >= NaN",
            "NaN <= NaN",
            "null < null",
            "'abc' < 'ABC'",
            "'abc' > 'ABC'",
            "'abc' < 1",
            "'abc' >= 1",
            "fromJSON('[1]') < fromJSON('[2]')",
            "fromJSON('[1]') <= fromJSON('[1]')",
            "github.event > 0",
            "github.event >= github.event",
        ],
    );
}

// ------------------------------------------------------- property access

#[test]
fn property_access() {
    assert_eq!(eval("github.event_name"), json!("push"));
    assert_eq!(eval("github['event_name']"), json!("push"));
    assert_eq!(eval("steps.my-step.outputs.result"), json!("ok"));
    assert_eq!(eval("steps['my-step'].outcome"), json!("success"));
    assert_eq!(eval("strategy.fail-fast"), json!(true));
    assert_eq!(eval("github.event.forced"), json!(false));
    assert_eq!(
        eval("github.event.issue"),
        json!({"labels": [{"name": "bug"}, {"name": "Help Wanted"}]})
    );
    assert_eq!(eval("(github.event).forced"), json!(false));
    assert_eq!(eval("fromJSON('{\"a\": {\"b\": 1}}').a.b"), json!(1));
}

#[test]
fn property_access_is_case_insensitive() {
    assert_eq!(eval("GITHUB.EVENT_NAME"), json!("push"));
    assert_eq!(eval("GitHub['Event_Name']"), json!("push"));
    assert_eq!(eval("env.my_var"), json!("hello"));
    // An exact match wins; otherwise the first case-insensitive match.
    assert_eq!(eval("vars.Name"), json!("upper"));
    assert_eq!(eval("vars.name"), json!("lower"));
    assert_eq!(eval("vars.NAME"), json!("upper"));
    assert_eq!(eval("vars.ONLY"), json!("x"));
}

#[test]
fn missing_properties_are_null() {
    assert_all(
        &Value::Null,
        &[
            "github.missing",
            "github.missing.deeper.still",
            "github.event.pull_request.number",
            "github.ref.length",
            "github.run_number.x",
            "github.event.commits.message",
            "vars.missing",
            "fromJSON('null').x",
        ],
    );
}

#[test]
fn index_access() {
    assert_eq!(eval("github.event.commits[0].message"), json!("first"));
    assert_eq!(
        eval("github['event']['commits'][0]['author']['name']"),
        json!("Mona")
    );
    // Indexes are coerced to numbers and floored.
    assert_eq!(eval("github.event.commits[1.9].message"), json!("second"));
    assert_eq!(eval("github.event.commits['1'].message"), json!("second"));
    assert_eq!(eval("github.event.commits[true].message"), json!("second"));
    assert_eq!(eval("fromJSON('[10, 20]')[1]"), json!(20));
    assert_eq!(eval("fromJSON('[10, 20]')[fromJSON('1')]"), json!(20));
    // Object keys are the index's string form.
    assert_eq!(eval("fromJSON('{\"1\": \"one\"}')[1]"), json!("one"));
    assert_eq!(eval("fromJSON('{\"true\": \"yes\"}')[true]"), json!("yes"));
    assert_eq!(
        eval("github[format('{0}_{1}', 'event', 'name')]"),
        json!("push")
    );
    assert_all(
        &Value::Null,
        &[
            "github.event.commits[5]",
            "github.event.commits[-1]",
            "github.event.commits['x']",
            "github.event.commits[NaN]",
            "github.event[fromJSON('[]')]",
            "github.event.commits[fromJSON('[0]')]",
        ],
    );
}

#[test]
fn index_access_on_literals_is_a_syntax_error() {
    assert!(matches!(eval_err("'abc'[0]"), Error::Parse { .. }));
    assert!(matches!(eval_err("'abc'.length"), Error::Parse { .. }));
}

// ------------------------------------------------------------ star filters

#[test]
fn star_filters() {
    let messages = json!(["first", "second"]);
    assert_eq!(eval("github.event.commits.*.message"), messages);
    assert_eq!(eval("github.event.commits[*].message"), messages);
    assert_eq!(eval("github.event.commits[*]['message']"), messages);
    assert_eq!(
        eval("github.event.issue.labels.*.name"),
        json!(["bug", "Help Wanted"])
    );
    assert_eq!(
        eval("github.event.commits.*.author.name"),
        json!(["Mona", "Hubot"])
    );
    assert_eq!(eval("matrix.*"), json!(["ubuntu-latest", 18, true]));
    assert_eq!(eval("github.event.commits.*"), eval("github.event.commits"));
    assert_eq!(
        eval("contains(github.event.issue.labels.*.name, 'BUG')"),
        json!(true)
    );
    assert_eq!(
        eval("contains(github.event.issue.labels.*.name, 'enhancement')"),
        json!(false)
    );
    assert_eq!(
        eval("join(github.event.commits.*.message, ', ')"),
        json!("first, second")
    );
}

#[test]
fn nested_star_filters() {
    let src = r#"fromJSON('[{"a": [{"b": 1}, {"b": 2}]}, {"a": [{"b": 3}]}, {"c": 4}]')"#;
    assert_eq!(eval(&format!("{src}.*.a.*.b")), json!([1, 2, 3]));
    assert_eq!(
        eval(&format!("{src}.*.a")),
        json!([[{"b": 1}, {"b": 2}], [{"b": 3}]])
    );
    assert_eq!(eval("fromJSON('[[1, 2], [3]]').*.*"), json!([1, 2, 3]));
    assert_eq!(eval("fromJSON('[[1, 2], [3], 4]').*[0]"), json!([1, 3]));
    assert_eq!(
        eval("fromJSON('{\"x\": {\"y\": 1}, \"z\": {\"y\": 2}}').*.y"),
        json!([1, 2])
    );
}

#[test]
fn star_filter_edge_cases() {
    // Missing keys are skipped, explicit nulls are kept (as on GitHub).
    assert_eq!(
        eval("fromJSON('[{\"a\": null}, {\"b\": 1}, {\"a\": 2}, 3]').*.a"),
        json!([null, 2])
    );
    // Objects have no integer indexes and arrays no string keys.
    assert_eq!(eval("github.event.commits.*[0]"), json!([]));
    assert_eq!(eval("fromJSON('[[1]]').*.a"), json!([]));
    // Filtering a non-collection gives an empty (but truthy) array.
    assert_eq!(eval("github.missing.*"), json!([]));
    assert_eq!(eval("github.event_name.*"), json!([]));
    assert_eq!(eval("github.missing.* && 'yes'"), json!("yes"));
    assert_eq!(eval("toJSON(github.event.commits.*.missing)"), json!("[]"));
    assert_eq!(eval("format('{0}', matrix.*)"), json!("Array"));
}

// ---------------------------------------------------------------- functions

#[test]
fn contains_function() {
    assert_all(
        &json!(true),
        &[
            "contains('Hello world', 'WORLD')",
            "contains('abc', '')",
            "contains(123, 2)",
            "contains(true, 'RU')",
            "contains(null, '')",
            "contains(github.ref, 'heads')",
            "contains(fromJSON('[\"a\", \"B\"]'), 'b')",
            "contains(fromJSON('[1, 2]'), '2')",
            "contains(fromJSON('[null]'), 0)",
            "contains(github.event.commits, github.event.commits[0])",
            "contains(fromJSON('[\"push\", \"pull_request\"]'), github.event_name)",
        ],
    );
    assert_all(
        &json!(false),
        &[
            "contains('abc', 'd')",
            "contains(fromJSON('[1, 2]'), 3)",
            "contains(fromJSON('[]'), 1)",
            "contains(fromJSON('[[1]]'), fromJSON('[1]'))",
            // Objects are not searched, and non-primitive items never match a string.
            "contains(github.event, 'commits')",
            "contains('Array', fromJSON('[]'))",
        ],
    );
}

#[test]
fn starts_with_and_ends_with() {
    assert_all(
        &json!(true),
        &[
            "startsWith('Hello world', 'he')",
            "startsWith(github.ref, 'refs/heads/')",
            "startsWith('abc', '')",
            "startsWith(12345, 12)",
            "endsWith('Hello world', 'WORLD')",
            "endsWith(true, 'UE')",
            "endsWith(github.repository, '/OCTO-REPO')",
        ],
    );
    assert_all(
        &json!(false),
        &[
            "startsWith('Hello world', 'world')",
            "endsWith('Hello world', 'hello')",
            "startsWith(fromJSON('[1]'), '')",
            "startsWith('abc', fromJSON('{}'))",
            "endsWith('a', 'ba')",
        ],
    );
}

#[test]
fn format_function() {
    assert_eq!(
        eval("format('Hello {0} {1} {2}', 'Mona', 'the', 'Octocat')"),
        json!("Hello Mona the Octocat")
    );
    assert_eq!(
        eval("format('{{Hello {0} {1} {2}!}}', 'Mona', 'the', 'Octocat')"),
        json!("{Hello Mona the Octocat!}")
    );
    assert_eq!(eval("format('{0}{0}{1}', 'a', 'b')"), json!("aab"));
    assert_eq!(eval("format('{{0}}')"), json!("{0}"));
    assert_eq!(eval("format('{0}}}', 'a')"), json!("a}"));
    assert_eq!(eval("format('{{{0}}}', 'a')"), json!("{a}"));
    assert_eq!(eval("format('no placeholders')"), json!("no placeholders"));
    assert_eq!(eval("format('')"), json!(""));
    assert_eq!(eval("format('{00}', 'x')"), json!("x"));
    assert_eq!(eval("format('{1}', 'unused', 'b')"), json!("b"));
    assert_eq!(
        eval("format('[{0}] [{1}] [{2}] [{3}] [{4}] [{5}]', null, true, 1.5, 100, fromJSON('[]'), github.event)"),
        json!("[] [true] [1.5] [100] [Array] [Object]")
    );
    assert_eq!(eval("format('{0}', 'héllo {1}')"), json!("héllo {1}"));
    assert_eq!(eval("format('é{0}ü', 1)"), json!("é1ü"));
    // Arguments are only evaluated when referenced.
    assert_eq!(eval("format('{0}', 'a', fromJSON('not json'))"), json!("a"));
}

#[test]
fn format_errors() {
    for src in [
        "format('{0', 1)",
        "format('{', 1)",
        "format('}', 1)",
        "format('a } b', 1)",
        "format('{a}', 1)",
        "format('{}', 1)",
        "format('{ 0}', 1)",
        "format('{256}', 1)",
    ] {
        let message = eval_message(src);
        assert!(
            message.contains("format string is invalid"),
            "{src}: {message}"
        );
    }
    let message = eval_message("format('{1}', 'a')");
    assert!(message.contains("references more arguments"), "{message}");
    let message = eval_message("format('{0}')");
    assert!(message.contains("references more arguments"), "{message}");
    let message = eval_message("format('{0:x}', 1)");
    assert!(message.contains("format specifiers 'x'"), "{message}");
    assert!(message.contains("Number"), "{message}");
}

#[test]
fn join_function() {
    assert_eq!(
        eval("join(fromJSON('[\"a\", \"b\", \"c\"]'))"),
        json!("a,b,c")
    );
    assert_eq!(
        eval("join(fromJSON('[\"a\", \"b\", \"c\"]'), ' | ')"),
        json!("a | b | c")
    );
    assert_eq!(
        eval("join(fromJSON('[1, null, true, 2.5, []]'), '-')"),
        json!("1--true-2.5-Array")
    );
    assert_eq!(eval("join(fromJSON('[\"only\"]'), '-')"), json!("only"));
    assert_eq!(eval("join(fromJSON('[]'), '-')"), json!(""));
    assert_eq!(
        eval("join(github.event.commits.*.message)"),
        json!("first,second")
    );
    assert_eq!(eval("join('abc', '-')"), json!("abc"));
    assert_eq!(eval("join(42)"), json!("42"));
    assert_eq!(eval("join(null)"), json!(""));
    assert_eq!(eval("join(github.event)"), json!(""));
    // A non-primitive separator falls back to ','; null is the empty string.
    assert_eq!(
        eval("join(fromJSON('[\"a\", \"b\"]'), fromJSON('{}'))"),
        json!("a,b")
    );
    assert_eq!(eval("join(fromJSON('[\"a\", \"b\"]'), null)"), json!("ab"));
    assert_eq!(eval("join(fromJSON('[\"a\", \"b\"]'), 0)"), json!("a0b"));
}

#[test]
fn to_json_function() {
    assert_eq!(
        eval("toJSON(fromJSON('{\"a\": [1, 2], \"b\": {}, \"c\": []}'))"),
        json!("{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {},\n  \"c\": []\n}")
    );
    assert_eq!(
        eval("toJSON(matrix)"),
        json!("{\n  \"os\": \"ubuntu-latest\",\n  \"node\": 18,\n  \"experimental\": true\n}")
    );
    assert_eq!(
        eval("toJSON(github.event.commits.*.message)"),
        json!("[\n  \"first\",\n  \"second\"\n]")
    );
    assert_eq!(eval("toJSON('x')"), json!("\"x\""));
    assert_eq!(eval("toJSON('say \"hi\"')"), json!("\"say \\\"hi\\\"\""));
    assert_eq!(eval("toJSON(null)"), json!("null"));
    assert_eq!(eval("toJSON(true)"), json!("true"));
    assert_eq!(eval("toJSON(3)"), json!("3"));
    assert_eq!(eval("toJSON(1.5)"), json!("1.5"));
}

#[test]
fn from_json_function() {
    assert_eq!(
        eval("fromJSON('{\"include\": [{\"os\": \"linux\"}]}')"),
        json!({"include": [{"os": "linux"}]})
    );
    assert_eq!(eval("fromJSON('true')"), json!(true));
    assert_eq!(eval("fromJSON('42')"), json!(42));
    assert_eq!(eval("fromJSON('-1.5')"), json!(-1.5));
    assert_eq!(eval("fromJSON('\"s\"')"), json!("s"));
    assert_eq!(eval("fromJSON('null')"), Value::Null);
    assert_eq!(eval("fromJSON(' [1] ')"), json!([1]));
    assert_eq!(eval("fromJSON('true') == true"), json!(true));
    // A matrix passed between jobs as a JSON string output.
    assert_eq!(
        eval("fromJSON(needs.build.outputs.matrix)"),
        json!({"os": ["linux", "windows"]})
    );
    assert_eq!(
        eval("fromJSON(needs.build.outputs.matrix).os[1]"),
        json!("windows")
    );
    assert_eq!(
        eval("fromJSON(needs.build.outputs.matrix).os.*"),
        json!(["linux", "windows"])
    );
}

#[test]
fn json_round_trip() {
    let event = eval("github.event");
    assert_eq!(eval("fromJSON(toJSON(github.event))"), event);
    assert_eq!(
        eval("toJSON(fromJSON(toJSON(github.event)))"),
        eval("toJSON(github.event)")
    );
    let text = eval("toJSON(github.event)");
    let parsed: Value = serde_json::from_str(text.as_str().unwrap()).unwrap();
    assert_eq!(parsed, event);
}

#[test]
fn from_json_errors_include_the_input() {
    let message = eval_message("fromJSON('{bad')");
    assert!(message.contains("{bad"), "{message}");
    assert!(matches!(eval_err("fromJSON('')"), Error::Eval(_)));
    assert!(matches!(eval_err("fromJSON('[1] x')"), Error::Eval(_)));
}

#[test]
fn function_names_are_case_insensitive() {
    assert_eq!(eval("CONTAINS('abc', 'B')"), json!(true));
    assert_eq!(eval("StartsWith('abc', 'A')"), json!(true));
    assert_eq!(eval("ENDSWITH('abc', 'C')"), json!(true));
    assert_eq!(eval("FORMAT('{0}', 1)"), json!("1"));
    assert_eq!(eval("JOIN(matrix.*, ' ')"), json!("ubuntu-latest 18 true"));
    assert_eq!(eval("toJson(1)"), json!("1"));
    assert_eq!(eval("FromJson('[1]')"), json!([1]));
}

#[test]
fn arity_errors() {
    let cases = [
        ("contains('a')", "Too few parameters supplied: 'contains'"),
        (
            "contains('a', 'b', 'c')",
            "Too many parameters supplied: 'contains'",
        ),
        (
            "startsWith('a')",
            "Too few parameters supplied: 'startsWith'",
        ),
        (
            "endsWith('a', 'b', 'c')",
            "Too many parameters supplied: 'endsWith'",
        ),
        ("format()", "Too few parameters supplied: 'format'"),
        ("join()", "Too few parameters supplied: 'join'"),
        ("join(1, 2, 3)", "Too many parameters supplied: 'join'"),
        ("toJSON()", "Too few parameters supplied: 'toJSON'"),
        ("toJSON(1, 2)", "Too many parameters supplied: 'toJSON'"),
        ("fromJSON()", "Too few parameters supplied: 'fromJSON'"),
    ];
    for (src, expected) in cases {
        assert_eq!(eval_message(src), expected, "{src}");
    }
}

#[test]
fn unknown_functions() {
    assert_eq!(eval_message("foo()"), "Unrecognized function: 'foo'");
    assert_eq!(eval_message("Foo(1, 2)"), "Unrecognized function: 'foo'");
    let contexts = contexts();
    let env = Env {
        contexts: &contexts,
        functions: &NoFunctions,
    };
    assert_eq!(
        eval_str("success()", &env),
        Err(Error::Eval("Unrecognized function: 'success'".into()))
    );
    assert_eq!(
        eval_str("hashFiles('**/Cargo.lock')", &env),
        Err(Error::Eval("Unrecognized function: 'hashfiles'".into()))
    );
}

#[test]
fn host_functions() {
    assert_eq!(
        eval("hashFiles('**/Cargo.lock')"),
        json!("hash(**/Cargo.lock)")
    );
    assert_eq!(
        eval("HASHFILES('a', format('{0}', 'b'))"),
        json!("hash(a,b)")
    );
    assert_eq!(
        eval("format('key-{0}', hashFiles('x'))"),
        json!("key-hash(x)")
    );
    assert_eq!(eval_message("hashFiles()"), "hashFiles requires a pattern");
    assert_eq!(eval("success()"), json!(true));
    assert_eq!(eval("failure()"), json!(false));
    assert_eq!(eval("always()"), json!(true));
    let failed = with_env(&Host::FAILED, |env| {
        eval_str("failure() && !success()", env)
    });
    assert_eq!(failed, Ok(json!(true)));
}

// ------------------------------------------------------------------ errors

#[test]
fn unknown_named_values() {
    assert_eq!(eval_message("foo.bar"), "Unrecognized named-value: 'foo'");
    assert_eq!(eval_message("FOO"), "Unrecognized named-value: 'foo'");
    assert_eq!(
        eval_message("format('{0}', unknown)"),
        "Unrecognized named-value: 'unknown'"
    );
    // Named values are checked up front, like GitHub does when parsing.
    assert_eq!(
        eval_message("false && foo"),
        "Unrecognized named-value: 'foo'"
    );
    // The first unknown name in source order is reported.
    assert_eq!(eval_message("a == b"), "Unrecognized named-value: 'a'");
}

#[test]
fn parse_error_positions() {
    let cases = [
        ("a b", 2),
        ("(1", 2),
        ("'abc", 0),
        ("x == 'abc", 5),
        ("1 == ", 5),
        ("contains(1,)", 11),
        ("a.1", 2),
        ("github.ref = 'x'", 11),
        ("github.ref && && x", 14),
        ("!", 1),
        ("", 0),
        ("1 2", 2),
        ("'é' ~ 1", 5),
    ];
    for (src, pos) in cases {
        let (_, actual, source) = parse_error(src);
        assert_eq!(actual, pos, "{src:?}");
        assert_eq!(source, src);
    }
}

#[test]
fn error_display() {
    let err = parse("a b").unwrap_err();
    assert_eq!(
        err.to_string(),
        "Unexpected symbol: 'b' (at position 2 in 'a b')"
    );
    assert_eq!(
        eval_err("foo").to_string(),
        "Unrecognized named-value: 'foo'"
    );
}

#[test]
fn deeply_nested_input_is_rejected_without_overflowing() {
    let deep = format!("{}1{}", "(".repeat(100_000), ")".repeat(100_000));
    assert!(matches!(parse(&deep), Err(Error::Parse { .. })));
    let chain = vec!["true"; 100_000].join(" && ");
    assert!(matches!(parse(&chain), Err(Error::Parse { .. })));
    // Realistic nesting is fine.
    let ok = format!("{}1{}", "(".repeat(40), ")".repeat(40));
    assert_eq!(eval(&ok), json!(1));
    let long_or = vec!["github.event_name == 'x'"; 100].join(" || ");
    assert_eq!(eval(&long_or), json!(false));
}

// ----------------------------------------------------------------- the API

#[test]
fn evaluate_a_parsed_expression_repeatedly() {
    let expr = parse("matrix.os == 'ubuntu-latest' && inputs.count").unwrap();
    let contexts = contexts();
    let env = Env {
        contexts: &contexts,
        functions: &NoFunctions,
    };
    assert_eq!(evaluate(&expr, &env), Ok(json!(3)));
    assert_eq!(evaluate(&expr, &env), Ok(json!(3)));
}

#[test]
fn parse_builds_the_documented_tree() {
    assert_eq!(
        parse("contains(github.event.issue.labels.*.name, 'bug')").unwrap(),
        Expr::Call(
            "contains".into(),
            vec![
                Expr::Index(
                    Box::new(Expr::Star(Box::new(Expr::Index(
                        Box::new(Expr::Index(
                            Box::new(Expr::Index(
                                Box::new(Expr::Context("github".into())),
                                Box::new(Expr::String("event".into())),
                            )),
                            Box::new(Expr::String("issue".into())),
                        )),
                        Box::new(Expr::String("labels".into())),
                    )))),
                    Box::new(Expr::String("name".into())),
                ),
                Expr::String("bug".into()),
            ],
        )
    );
}
