mod common;

use baste_expr::{
    evaluate_condition, has_expression, interpolate, interpolate_value, is_truthy,
    to_display_string, Env, Error, NoFunctions, Value,
};
use common::{contexts, with_env, Host};
use serde_json::json;

fn render(template: &str) -> String {
    with_env(&Host::SUCCESS, |env| interpolate(template, env))
        .unwrap_or_else(|e| panic!("interpolating {template:?} failed: {e}"))
}

fn render_value(template: &str) -> Value {
    with_env(&Host::SUCCESS, |env| interpolate_value(template, env))
        .unwrap_or_else(|e| panic!("interpolating {template:?} failed: {e}"))
}

fn condition(host: &Host, cond: &str) -> bool {
    with_env(host, |env| evaluate_condition(cond, env))
        .unwrap_or_else(|e| panic!("condition {cond:?} failed: {e}"))
}

// ------------------------------------------------------------- interpolate

#[test]
fn interpolates_multiple_expressions() {
    assert_eq!(
        render("Hello ${{ github.event_name }} on ${{ matrix.os }}!"),
        "Hello push on ubuntu-latest!"
    );
    assert_eq!(render("${{1}}${{ 2 }}"), "12");
    assert_eq!(
        render("line1\n  ${{ matrix.os }}\nline3 ${{ secrets.TOKEN }}\n"),
        "line1\n  ubuntu-latest\nline3 s3cret\n"
    );
    assert_eq!(render("héllo ${{ 'wörld' }} ✓"), "héllo wörld ✓");
}

#[test]
fn text_without_expressions_is_unchanged() {
    for text in [
        "",
        "plain",
        "  spaced  ",
        "$ {{ x }}",
        "${ {x}}",
        "{{ x }}",
        "$${ x }}",
        "}}",
    ] {
        assert_eq!(render(text), text);
    }
}

#[test]
fn closing_braces_inside_string_literals() {
    assert_eq!(render("${{ format('{{0}}') }}"), "{0}");
    assert_eq!(render("a ${{ '}}' }} b"), "a }} b");
    assert_eq!(render("${{ 'it''s }}' }}"), "it's }}");
    assert_eq!(render("${{ format('{{{0}}}', 'x') }}!"), "{x}!");
    assert_eq!(
        render("${{ '{{ this is a literal }}' }}"),
        "{{ this is a literal }}"
    );
    assert_eq!(render("${{ 'a' }}}"), "a}");
    assert_eq!(render("${{ '${{ x }}' }}"), "${{ x }}");
}

#[test]
fn values_are_converted_to_display_strings() {
    assert_eq!(render("${{ github.event.commits }}"), "Array");
    assert_eq!(render("${{ github.event }}"), "Object");
    assert_eq!(render("[${{ null }}]"), "[]");
    assert_eq!(render("[${{ github.missing }}]"), "[]");
    assert_eq!(render("${{ true }}/${{ false }}"), "true/false");
    assert_eq!(render("${{ github.run_number }}"), "42");
    assert_eq!(
        render("${{ 1.50 }} ${{ 100 }} ${{ -7 }} ${{ 0.1 }}"),
        "1.5 100 -7 0.1"
    );
    assert_eq!(render("${{ 1e21 }} ${{ 1e-7 }} ${{ -0 }}"), "1e+21 1e-7 0");
    assert_eq!(
        render("${{ NaN }} ${{ Infinity }} ${{ -Infinity }}"),
        "NaN Infinity -Infinity"
    );
}

#[test]
fn unterminated_expression_is_a_parse_error() {
    let err = with_env(&Host::SUCCESS, |env| {
        interpolate("echo ${{ github.ref", env)
    })
    .unwrap_err();
    let Error::Parse {
        message,
        pos,
        source_text,
    } = err
    else {
        panic!("expected a parse error, got {err:?}");
    };
    assert_eq!(pos, 5);
    assert_eq!(source_text, "echo ${{ github.ref");
    assert!(message.contains("Unterminated"), "{message}");

    // The `}}` is inside an unclosed string literal.
    let err = with_env(&Host::SUCCESS, |env| interpolate("${{ 'abc }}", env)).unwrap_err();
    assert!(matches!(err, Error::Parse { pos: 0, .. }), "{err:?}");
}

#[test]
fn syntax_errors_point_into_the_expression() {
    let err = with_env(&Host::SUCCESS, |env| {
        interpolate("x ${{  github.ref == \"a\" }}", env)
    })
    .unwrap_err();
    assert_eq!(
        err,
        Error::Parse {
            message: "Unexpected symbol: '\"a\"'. String literals must use single quotes".into(),
            pos: 14,
            source_text: "github.ref == \"a\"".into(),
        }
    );
    let err = with_env(&Host::SUCCESS, |env| interpolate("${{ }}", env)).unwrap_err();
    assert!(matches!(err, Error::Parse { .. }), "{err:?}");
}

#[test]
fn syntax_errors_are_reported_before_evaluation() {
    // The first block would fail to evaluate, but the second does not parse.
    let err = with_env(&Host::SUCCESS, |env| {
        interpolate("${{ foo }} ${{ a b }}", env)
    })
    .unwrap_err();
    assert!(matches!(err, Error::Parse { .. }), "{err:?}");
}

#[test]
fn evaluation_errors_propagate() {
    let err = with_env(&Host::SUCCESS, |env| interpolate("${{ foo }}", env)).unwrap_err();
    assert_eq!(err, Error::Eval("Unrecognized named-value: 'foo'".into()));
}

#[test]
fn has_expression_detects_blocks() {
    assert!(has_expression("${{ x }}"));
    assert!(has_expression("a ${{"));
    assert!(!has_expression("a ${ { x }}"));
    assert!(!has_expression("plain"));
}

// ------------------------------------------------------- interpolate_value

#[test]
fn single_expression_keeps_its_type() {
    assert_eq!(
        render_value("${{ github.event.commits.*.message }}"),
        json!(["first", "second"])
    );
    assert_eq!(render_value("${{ matrix.node }}"), json!(18));
    assert_eq!(render_value("  ${{ matrix.experimental }}\n"), json!(true));
    assert_eq!(
        render_value("${{ fromJSON('{\"a\": 1}') }}"),
        json!({"a": 1})
    );
    assert_eq!(
        render_value("${{ matrix }}"),
        json!({"os": "ubuntu-latest", "node": 18, "experimental": true})
    );
    assert_eq!(render_value("${{ null }}"), Value::Null);
    assert_eq!(render_value("${{ 1.5 }}"), json!(1.5));
    assert_eq!(render_value("${{ '}}' }}"), json!("}}"));
}

#[test]
fn anything_else_becomes_a_string() {
    assert_eq!(render_value("node-${{ matrix.node }}"), json!("node-18"));
    assert_eq!(render_value("${{ 1 }}${{ 2 }}"), json!("12"));
    assert_eq!(render_value("${{ 1 }} x"), json!("1 x"));
    assert_eq!(render_value("plain"), json!("plain"));
    assert_eq!(render_value("  plain  "), json!("  plain  "));
    assert_eq!(render_value(""), json!(""));
}

#[test]
fn interpolate_value_errors() {
    let err = with_env(&Host::SUCCESS, |env| {
        interpolate_value("${{ github.ref", env)
    })
    .unwrap_err();
    assert!(matches!(err, Error::Parse { .. }), "{err:?}");
    let err = with_env(&Host::SUCCESS, |env| interpolate_value("${{ nope }}", env)).unwrap_err();
    assert!(matches!(err, Error::Eval(_)), "{err:?}");
}

// ------------------------------------------------------ evaluate_condition

#[test]
fn bare_and_wrapped_conditions() {
    let ok = Host::SUCCESS;
    assert!(condition(&ok, "github.event_name == 'push'"));
    assert!(!condition(&ok, "github.event_name == 'pull_request'"));
    assert!(condition(&ok, "${{ github.event_name == 'push' }}"));
    assert!(condition(&ok, "  ${{ github.event_name == 'push' }}\n"));
    assert!(!condition(&ok, "${{ false }}"));
    assert!(!condition(
        &ok,
        "${{ github.event_name == 'pull_request' }}"
    ));
    assert!(condition(&ok, "${{ '}}' == '}}' }}"));
}

#[test]
fn condition_results_use_truthiness() {
    let ok = Host::SUCCESS;
    assert!(condition(&ok, "'abc'"));
    assert!(!condition(&ok, "0"));
    assert!(!condition(&ok, "''"));
    assert!(condition(&ok, "github.event.commits"));
    assert!(condition(&ok, "fromJSON('[]')"));
    assert!(!condition(&ok, "env.EMPTY"));
    assert!(condition(&ok, "env.ZERO"));
    assert!(!condition(&ok, "github.missing"));
    assert!(!condition(&ok, "NaN"));
}

#[test]
fn empty_condition_means_success() {
    assert!(condition(&Host::SUCCESS, ""));
    assert!(condition(&Host::SUCCESS, "  \n "));
    assert!(!condition(&Host::FAILED, ""));
    assert!(!condition(&Host::CANCELLED, ""));
}

#[test]
fn implicit_success_is_added() {
    let failed = Host::FAILED;
    assert!(!condition(&failed, "true"));
    assert!(!condition(&failed, "${{ true }}"));
    assert!(!condition(&failed, "github.event_name == 'push'"));
    assert!(!condition(&Host::CANCELLED, "true"));
}

#[test]
fn status_functions_disable_implicit_success() {
    let failed = Host::FAILED;
    assert!(condition(&failed, "always()"));
    assert!(condition(&failed, "${{ always() && true }}"));
    assert!(condition(&failed, "failure()"));
    assert!(condition(
        &failed,
        "failure() && github.event_name == 'push'"
    ));
    assert!(condition(&failed, "success() || failure()"));
    assert!(!condition(&failed, "success()"));
    assert!(condition(&failed, "!cancelled()"));
    assert!(condition(&failed, "Failure()"));
    // Found anywhere in the tree, e.g. inside function arguments.
    assert!(condition(
        &failed,
        "contains(format('{0}', failure()), 'true')"
    ));

    assert!(!condition(&Host::SUCCESS, "failure()"));
    assert!(condition(&Host::CANCELLED, "cancelled()"));
    assert!(!condition(&Host::CANCELLED, "!cancelled()"));
}

#[test]
fn string_templates_are_truthy_when_non_empty() {
    let ok = Host::SUCCESS;
    // A classic mistake: this interpolates to the string "false && x".
    assert!(condition(&ok, "${{ false }} && x"));
    assert!(condition(
        &ok,
        "${{ github.event_name == 'nope' }} || false"
    ));
    assert!(condition(&ok, "x ${{ '' }}"));
    assert!(!condition(&ok, "${{ '' }}${{ github.missing }}"));
}

#[test]
fn string_templates_still_require_success() {
    // GitHub wraps the interpolated string in `success() && (...)` too...
    assert!(!condition(&Host::FAILED, "${{ true }} && x"));
    // ...unless one of the blocks calls a status function.
    assert!(condition(&Host::FAILED, "${{ always() }} x"));
    assert!(!condition(&Host::FAILED, "${{ '' }}${{ always() && '' }}"));
}

#[test]
fn condition_errors() {
    let err = with_env(&Host::SUCCESS, |env| evaluate_condition("${{ foo }}", env)).unwrap_err();
    assert_eq!(err, Error::Eval("Unrecognized named-value: 'foo'".into()));
    let err = with_env(&Host::SUCCESS, |env| evaluate_condition("a b", env)).unwrap_err();
    assert!(matches!(err, Error::Parse { pos: 2, .. }), "{err:?}");
    let err = with_env(&Host::SUCCESS, |env| evaluate_condition("${{ }}", env)).unwrap_err();
    assert!(matches!(err, Error::Parse { .. }), "{err:?}");
    let err = with_env(&Host::SUCCESS, |env| evaluate_condition("${{ x", env)).unwrap_err();
    assert!(matches!(err, Error::Parse { .. }), "{err:?}");

    // Without host functions, the implicit success() cannot be evaluated.
    let contexts = contexts();
    let env = Env {
        contexts: &contexts,
        functions: &NoFunctions,
    };
    assert_eq!(
        evaluate_condition("true", &env),
        Err(Error::Eval("Unrecognized function: 'success'".into()))
    );
}

// ------------------------------------------------------ value conversions

#[test]
fn display_strings() {
    let cases = [
        (Value::Null, ""),
        (json!(true), "true"),
        (json!(false), "false"),
        (json!(3), "3"),
        (json!(-7), "-7"),
        (json!(1.0), "1"),
        (json!(1.5), "1.5"),
        (json!(0.1), "0.1"),
        (json!(-0.0), "0"),
        (json!(1e21), "1e+21"),
        (json!(1e-7), "1e-7"),
        (json!(123456789.125), "123456789.125"),
        (json!(1e15), "1000000000000000"),
        (json!(u64::MAX), "18446744073709551615"),
        (json!(i64::MIN), "-9223372036854775808"),
        (json!("text"), "text"),
        (json!(""), ""),
        (json!([]), "Array"),
        (json!([1, 2]), "Array"),
        (json!({}), "Object"),
        (json!({"a": 1}), "Object"),
    ];
    for (value, expected) in cases {
        assert_eq!(to_display_string(&value), expected, "{value:?}");
    }
}

#[test]
fn truthiness() {
    for falsy in [
        Value::Null,
        json!(false),
        json!(0),
        json!(0.0),
        json!(-0.0),
        json!(""),
    ] {
        assert!(!is_truthy(&falsy), "{falsy:?}");
    }
    for truthy in [
        json!(true),
        json!(1),
        json!(-1),
        json!(0.5),
        json!("0"),
        json!("false"),
        json!(" "),
        json!([]),
        json!({}),
        json!([0]),
    ] {
        assert!(is_truthy(&truthy), "{truthy:?}");
    }
}
