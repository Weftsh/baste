//! YAML to JSON conversion.
//!
//! Workflows are parsed with YAML 1.2 core-schema rules (so `on` is a string,
//! not a boolean) and converted to `serde_json::Value` with key order kept.
//! Floats keep GitHub's behaviour: `3.10` becomes the number `3.1`.

use serde_json::{Map, Number, Value};
use yaml_rust2::{Yaml, YamlLoader};

/// Parse the first YAML document in `src` into JSON.
pub fn parse(src: &str) -> Result<Value, String> {
    let docs = YamlLoader::load_from_str(src).map_err(|e| e.to_string())?;
    match docs.into_iter().next() {
        Some(doc) => to_json(&doc),
        None => Ok(Value::Null),
    }
}

fn to_json(y: &Yaml) -> Result<Value, String> {
    Ok(match y {
        Yaml::Null => Value::Null,
        Yaml::Boolean(b) => Value::Bool(*b),
        Yaml::Integer(i) => Value::Number((*i).into()),
        Yaml::Real(s) => match s.parse::<f64>().ok().and_then(Number::from_f64) {
            Some(n) => Value::Number(n),
            None => Value::String(s.clone()),
        },
        Yaml::String(s) => Value::String(s.clone()),
        Yaml::Array(items) => Value::Array(items.iter().map(to_json).collect::<Result<_, _>>()?),
        Yaml::Hash(h) => {
            let mut map = Map::new();
            for (k, v) in h {
                map.insert(key_string(k)?, to_json(v)?);
            }
            Value::Object(map)
        }
        Yaml::Alias(_) => return Err("unresolved YAML alias".into()),
        Yaml::BadValue => return Err("invalid YAML value".into()),
    })
}

fn key_string(k: &Yaml) -> Result<String, String> {
    Ok(match k {
        Yaml::String(s) | Yaml::Real(s) => s.clone(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Boolean(b) => b.to_string(),
        Yaml::Null => "null".into(),
        _ => return Err("mapping keys must be scalars".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_scalars_and_keeps_order() {
        let v = parse("on: push\nb: 1\na: [3.10, true, ~, 'x']\n").unwrap();
        assert_eq!(
            v,
            json!({"on": "push", "b": 1, "a": [3.1, true, null, "x"]})
        );
        let keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, vec!["on", "b", "a"]);
    }

    #[test]
    fn resolves_anchors() {
        let v = parse("x: &a {k: 1}\ny: *a\n").unwrap();
        assert_eq!(v["y"], json!({"k": 1}));
    }

    #[test]
    fn reports_errors() {
        assert!(parse("a: [1, 2").is_err());
    }
}
