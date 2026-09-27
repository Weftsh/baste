//! `action.yml` metadata.

use crate::model::scalar_string;
use crate::yaml;
use serde_json::{Map, Value};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct ActionInput {
    pub name: String,
    pub default: Option<Value>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Runs {
    /// A JavaScript action. `using` is normalised to a runtime we ship
    /// (`node20` or `node24`); older runtimes run on `node20`, as on GitHub.
    Node {
        using: String,
        main: String,
        pre: Option<String>,
        pre_if: Option<String>,
        post: Option<String>,
        post_if: Option<String>,
    },
    Composite {
        steps: Vec<Value>,
    },
    Docker {
        /// `Dockerfile`, a path to one, or `docker://image`.
        image: String,
        args: Vec<Value>,
        entrypoint: Option<String>,
        pre_entrypoint: Option<String>,
        post_entrypoint: Option<String>,
        env: Map<String, Value>,
        pre_if: Option<String>,
        post_if: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActionMeta {
    pub name: String,
    pub inputs: Vec<ActionInput>,
    /// Output definitions. For composite actions each has a `value` template.
    pub outputs: Map<String, Value>,
    pub runs: Runs,
}

impl ActionMeta {
    /// Read `action.yml` (or `action.yaml`) from `dir`.
    pub fn load(dir: &Path) -> Result<ActionMeta, String> {
        for name in ["action.yml", "action.yaml"] {
            let p = dir.join(name);
            if p.is_file() {
                let src = std::fs::read_to_string(&p)
                    .map_err(|e| format!("reading {}: {e}", p.display()))?;
                return ActionMeta::parse(&src).map_err(|e| format!("{}: {e}", p.display()));
            }
        }
        if dir.join("Dockerfile").is_file() {
            // Actions without metadata that ship a Dockerfile are allowed.
            return Ok(ActionMeta {
                name: dir.display().to_string(),
                inputs: vec![],
                outputs: Map::new(),
                runs: Runs::Docker {
                    image: "Dockerfile".into(),
                    args: vec![],
                    entrypoint: None,
                    pre_entrypoint: None,
                    post_entrypoint: None,
                    env: Map::new(),
                    pre_if: None,
                    post_if: None,
                },
            });
        }
        Err(format!("no action.yml or action.yaml in {}", dir.display()))
    }

    pub fn parse(src: &str) -> Result<ActionMeta, String> {
        let root = yaml::parse(src)?;
        let obj = root
            .as_object()
            .ok_or("action metadata must be a mapping")?;
        let mut inputs = Vec::new();
        if let Some(Value::Object(map)) = obj.get("inputs") {
            for (name, def) in map {
                inputs.push(ActionInput {
                    name: name.clone(),
                    default: def.get("default").filter(|v| !v.is_null()).cloned(),
                    required: def
                        .get("required")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
            }
        }
        let runs = obj
            .get("runs")
            .and_then(Value::as_object)
            .ok_or("missing 'runs'")?;
        let s = |k: &str| runs.get(k).and_then(scalar_string);
        let using = s("using").ok_or("missing 'runs.using'")?;
        let runs = match using.to_ascii_lowercase().as_str() {
            "node12" | "node16" | "node20" | "node24" => Runs::Node {
                using: if using.eq_ignore_ascii_case("node24") {
                    "node24".into()
                } else {
                    "node20".into()
                },
                main: s("main").ok_or("missing 'runs.main'")?,
                pre: s("pre"),
                pre_if: s("pre-if"),
                post: s("post"),
                post_if: s("post-if"),
            },
            "composite" => Runs::Composite {
                steps: runs
                    .get("steps")
                    .and_then(Value::as_array)
                    .cloned()
                    .ok_or("missing 'runs.steps'")?,
            },
            "docker" => Runs::Docker {
                image: s("image").ok_or("missing 'runs.image'")?,
                args: runs
                    .get("args")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                entrypoint: s("entrypoint"),
                pre_entrypoint: s("pre-entrypoint"),
                post_entrypoint: s("post-entrypoint"),
                env: runs
                    .get("env")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default(),
                pre_if: s("pre-if"),
                post_if: s("post-if"),
            },
            other => return Err(format!("unsupported 'runs.using: {other}'")),
        };
        Ok(ActionMeta {
            name: obj.get("name").and_then(scalar_string).unwrap_or_default(),
            inputs,
            outputs: obj
                .get("outputs")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            runs,
        })
    }

    /// Remote actions referenced by a composite action's steps.
    pub fn nested_uses(&self) -> Vec<String> {
        match &self.runs {
            Runs::Composite { steps } => steps
                .iter()
                .filter_map(|s| s.get("uses").and_then(scalar_string))
                .collect(),
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_node_action() {
        let a = ActionMeta::parse(
            "name: Setup\ninputs:\n  version:\n    default: 20\n    required: false\nruns:\n  using: node16\n  main: dist/index.js\n  post: dist/post.js\n",
        )
        .unwrap();
        assert_eq!(a.inputs[0].default, Some(serde_json::json!(20)));
        match a.runs {
            Runs::Node {
                using, main, post, ..
            } => {
                assert_eq!(using, "node20");
                assert_eq!(main, "dist/index.js");
                assert_eq!(post.as_deref(), Some("dist/post.js"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parses_composite_and_docker() {
        let a = ActionMeta::parse(
            "name: C\nruns:\n  using: composite\n  steps:\n    - run: echo hi\n      shell: bash\n    - uses: actions/cache@v4\n",
        )
        .unwrap();
        assert_eq!(a.nested_uses(), vec!["actions/cache@v4"]);
        let d = ActionMeta::parse(
            "name: D\nruns:\n  using: docker\n  image: Dockerfile\n  args: ['${{ inputs.x }}']\n",
        )
        .unwrap();
        assert!(matches!(d.runs, Runs::Docker { .. }));
        assert!(ActionMeta::parse("runs:\n  using: python\n").is_err());
    }
}
