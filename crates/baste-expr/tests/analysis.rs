use baste_expr::{
    contains_status_function, parse, referenced_properties, referenced_secrets, Expr,
};

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(c, p)| (c.to_string(), p.to_string()))
        .collect()
}

#[test]
fn secrets_in_blocks() {
    assert_eq!(referenced_secrets("${{ secrets.A }}", false), ["A"]);
    assert_eq!(referenced_secrets("${{ secrets['B'] }}", false), ["B"]);
    assert_eq!(
        referenced_secrets("${{ secrets [ 'B C' ] }}", false),
        ["B C"]
    );
    assert_eq!(
        referenced_secrets(
            "token: ${{ secrets.GITHUB_TOKEN }} / ${{ secrets.npm-token }}",
            false
        ),
        ["GITHUB_TOKEN", "npm-token"]
    );
}

#[test]
fn nested_secret_references() {
    let text = "${{ format('{0}-{1}', secrets.C, contains(secrets['D'], secrets.E) && secrets.F == 'x') }}";
    assert_eq!(referenced_secrets(text, false), ["C", "D", "E", "F"]);
    assert_eq!(
        referenced_secrets("${{ !secrets.G || (secrets.H) }}", false),
        ["G", "H"]
    );
    assert_eq!(
        referenced_secrets("${{ github.event[secrets.K] }}", false),
        ["K"]
    );
    assert_eq!(
        referenced_secrets("${{ fromJSON(secrets.JSON).field }}", false),
        ["JSON"]
    );
    assert_eq!(referenced_secrets("${{ secrets.L.nested }}", false), ["L"]);
}

#[test]
fn secrets_are_deduplicated_in_first_seen_order() {
    assert_eq!(
        referenced_secrets(
            "${{ secrets.X }} ${{ secrets.Y }} ${{ secrets['X'] }} ${{ secrets.W }}",
            false
        ),
        ["X", "Y", "W"]
    );
    // Names are returned as written; the context name is case-insensitive.
    assert_eq!(
        referenced_secrets("${{ secrets.my_Token }} ${{ SECRETS.Other }}", false),
        ["my_Token", "Other"]
    );
}

#[test]
fn things_that_are_not_secret_references() {
    for text in [
        "${{ github.secrets.A }}",
        "${{ 'secrets.A' }}",
        "secrets.A",
        "${{ secrets }}",
        "${{ toJSON(secrets) }}",
        "${{ secrets.*.x }}",
        "${{ secrets[format('{0}', 'X')] }}",
        "${{ secrets[0] }}",
        "",
    ] {
        assert!(referenced_secrets(text, false).is_empty(), "{text}");
    }
}

#[test]
fn bare_expressions() {
    assert_eq!(referenced_secrets("secrets.A != ''", true), ["A"]);
    assert_eq!(
        referenced_secrets("secrets.A != '' && secrets['B']", true),
        ["A", "B"]
    );
    // Bare text containing `${{` is scanned as a template, like `if:` on GitHub.
    assert_eq!(referenced_secrets("${{ secrets.A }}", true), ["A"]);
    assert!(referenced_secrets("secrets.A", false).is_empty());
}

#[test]
fn parse_errors_are_ignored() {
    assert_eq!(
        referenced_secrets("${{ secrets.A == \"x\" }} ${{ secrets.B }}", false),
        ["B"]
    );
    // Blocks before an unterminated `${{` are still reported.
    assert_eq!(
        referenced_secrets("${{ secrets.A }} ${{ secrets.B", false),
        ["A"]
    );
    assert!(referenced_secrets("secrets.A ==", true).is_empty());
}

#[test]
fn properties_of_any_context() {
    let text = "${{ needs.build.outputs.version }} ${{ steps['test'].outcome }} ${{ matrix.os }}";
    assert_eq!(
        referenced_properties(text, false),
        pairs(&[("needs", "build"), ("steps", "test"), ("matrix", "os")])
    );
    assert_eq!(
        referenced_properties("needs.a.result == 'success' && NEEDS.B.result", true),
        pairs(&[("needs", "a"), ("needs", "B")])
    );
    assert_eq!(
        referenced_properties(
            "${{ format('{0}', needs.x.outputs.y, needs.x.result) }}",
            false
        ),
        pairs(&[("needs", "x")])
    );
    assert_eq!(
        referenced_properties("${{ contains(needs.*.result, 'failure') || needs }}", false),
        pairs(&[])
    );
    assert_eq!(
        referenced_properties("${{ github.event.pull_request.head.sha }}", false),
        pairs(&[("github", "event")])
    );
}

#[test]
fn status_function_detection() {
    for src in [
        "success()",
        "always()",
        "x && Always()",
        "contains(failure(), 'x')",
        "!cancelled()",
        "format('{0}', github[success()])",
    ] {
        assert!(contains_status_function(&parse(src).unwrap()), "{src}");
    }
    for src in [
        "github.success",
        "'success()'",
        "hashFiles('x')",
        "true",
        "successes()",
    ] {
        assert!(!contains_status_function(&parse(src).unwrap()), "{src}");
    }
    // Hand-built trees may use any case.
    assert!(contains_status_function(&Expr::Call(
        "Success".into(),
        vec![]
    )));
}
