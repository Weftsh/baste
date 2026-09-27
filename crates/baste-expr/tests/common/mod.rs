//! Shared fixtures for the integration tests.

#![allow(dead_code)]

use baste_expr::{eval_str, to_display_string, Env, Error, Functions, Map, Value};
use serde_json::json;

pub fn contexts() -> Map<String, Value> {
    let Value::Object(map) = json!({
        "github": {
            "event_name": "push",
            "ref": "refs/heads/main",
            "repository": "octo-org/octo-repo",
            "run_number": 42,
            "event": {
                "commits": [
                    {"message": "first", "author": {"name": "Mona"}},
                    {"message": "second", "author": {"name": "Hubot"}},
                    {"id": 3}
                ],
                "issue": {"labels": [{"name": "bug"}, {"name": "Help Wanted"}]},
                "pull_request": null,
                "forced": false,
                "size": 1.5
            }
        },
        "env": {"MY_VAR": "hello", "EMPTY": "", "ZERO": "0"},
        "vars": {"Name": "upper", "name": "lower", "only": "x"},
        "matrix": {"os": "ubuntu-latest", "node": 18, "experimental": true},
        "steps": {"my-step": {"outputs": {"result": "ok"}, "outcome": "success"}},
        "needs": {
            "build": {
                "result": "success",
                "outputs": {"version": "1.2.3", "matrix": "{\"os\": [\"linux\", \"windows\"]}"}
            }
        },
        "secrets": {"TOKEN": "s3cret"},
        "inputs": {"flag": true, "count": 3},
        "strategy": {"fail-fast": true},
        "runner": {"os": "Linux"},
        "job": {"status": "success"}
    }) else {
        unreachable!()
    };
    map
}

/// Status functions and `hashFiles`, as a runner would provide them.
#[derive(Debug, Default, Clone, Copy)]
pub struct Host {
    pub failed: bool,
    pub cancelled: bool,
}

impl Host {
    pub const SUCCESS: Host = Host {
        failed: false,
        cancelled: false,
    };
    pub const FAILED: Host = Host {
        failed: true,
        cancelled: false,
    };
    pub const CANCELLED: Host = Host {
        failed: false,
        cancelled: true,
    };
}

impl Functions for Host {
    fn call(&self, name: &str, args: &[Value]) -> Option<Result<Value, Error>> {
        let status = match name {
            "success" => !self.failed && !self.cancelled,
            "failure" => self.failed,
            "cancelled" => self.cancelled,
            "always" => true,
            "hashfiles" => {
                return Some(if args.is_empty() {
                    Err(Error::Eval("hashFiles requires a pattern".into()))
                } else {
                    let args: Vec<_> = args.iter().map(to_display_string).collect();
                    Ok(Value::String(format!("hash({})", args.join(","))))
                });
            }
            _ => return None,
        };
        Some(Ok(Value::Bool(status)))
    }
}

/// Runs `f` with an environment over [`contexts`] and `host`.
pub fn with_env<T>(host: &Host, f: impl FnOnce(&Env) -> T) -> T {
    let contexts = contexts();
    let env = Env {
        contexts: &contexts,
        functions: host,
    };
    f(&env)
}

pub fn eval(src: &str) -> Value {
    with_env(&Host::SUCCESS, |env| eval_str(src, env))
        .unwrap_or_else(|e| panic!("evaluating {src:?} failed: {e}"))
}

pub fn eval_err(src: &str) -> Error {
    match with_env(&Host::SUCCESS, |env| eval_str(src, env)) {
        Ok(v) => panic!("evaluating {src:?} should fail, got {v}"),
        Err(e) => e,
    }
}

/// The message of an [`Error::Eval`].
pub fn eval_message(src: &str) -> String {
    match eval_err(src) {
        Error::Eval(message) => message,
        other => panic!("expected an evaluation error for {src:?}, got {other:?}"),
    }
}

pub fn parse_error(src: &str) -> (String, usize, String) {
    match eval_err(src) {
        Error::Parse {
            message,
            pos,
            source_text,
        } => (message, pos, source_text),
        other => panic!("expected a parse error for {src:?}, got {other:?}"),
    }
}
