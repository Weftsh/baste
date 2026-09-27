//! Workflow commands printed by steps (`::add-mask::`, `::error::`, ...) and
//! the environment files steps write (`GITHUB_ENV`, `GITHUB_OUTPUT`, ...).

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub properties: BTreeMap<String, String>,
    pub message: String,
}

/// Parse `::name key=value,key=value::message`. Returns `None` for ordinary lines.
pub fn parse_command(line: &str) -> Option<Command> {
    let rest = line.trim_start().strip_prefix("::")?;
    let (head, message) = rest.split_once("::")?;
    let (name, props) = match head.split_once(' ') {
        Some((n, p)) => (n, p),
        None => (head, ""),
    };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let mut properties = BTreeMap::new();
    for pair in props.split(',').filter(|p| !p.trim().is_empty()) {
        if let Some((k, v)) = pair.split_once('=') {
            properties.insert(k.trim().to_string(), unescape_property(v));
        }
    }
    Some(Command {
        name: name.to_string(),
        properties,
        message: unescape_data(message),
    })
}

fn unescape_data(s: &str) -> String {
    s.replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%25", "%")
}

fn unescape_property(s: &str) -> String {
    s.replace("%0D", "\r")
        .replace("%0A", "\n")
        .replace("%3A", ":")
        .replace("%2C", ",")
        .replace("%25", "%")
}

/// Parse a `GITHUB_ENV` / `GITHUB_OUTPUT` / `GITHUB_STATE` file: `key=value`
/// lines and `key<<DELIMITER` heredocs. Later keys win.
pub fn parse_env_file(content: &str) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut lines = content
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l));
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let eq = line.find('=');
        let heredoc = line.find("<<");
        match (eq, heredoc) {
            (Some(e), h) if h.is_none_or(|h| e < h) => {
                let key = &line[..e];
                if key.is_empty() {
                    return Err(format!("invalid line (empty name): {line}"));
                }
                out.push((key.to_string(), line[e + 1..].to_string()));
            }
            (_, Some(h)) => {
                let key = &line[..h];
                let delimiter = &line[h + 2..];
                if key.is_empty() || delimiter.is_empty() {
                    return Err(format!("invalid heredoc line: {line}"));
                }
                let mut value: Vec<&str> = Vec::new();
                let mut closed = false;
                for l in lines.by_ref() {
                    if l == delimiter {
                        closed = true;
                        break;
                    }
                    value.push(l);
                }
                if !closed {
                    return Err(format!(
                        "matching delimiter '{delimiter}' not found for '{key}'"
                    ));
                }
                out.push((key.to_string(), value.join("\n")));
            }
            _ => return Err(format!("invalid line (expected name=value): {line}")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        let c =
            parse_command("::error file=app.js,line=10,title=Oops%2C bad::Something%0Awent wrong")
                .unwrap();
        assert_eq!(c.name, "error");
        assert_eq!(c.properties["file"], "app.js");
        assert_eq!(c.properties["line"], "10");
        assert_eq!(c.properties["title"], "Oops, bad");
        assert_eq!(c.message, "Something\nwent wrong");
        assert_eq!(
            parse_command("::add-mask::s3cret").unwrap().message,
            "s3cret"
        );
        assert_eq!(parse_command("::endgroup::").unwrap().name, "endgroup");
        assert!(parse_command("hello ::x::").is_none());
        assert!(parse_command("::not a command").is_none());
        assert!(parse_command(":: ::x").is_none());
    }

    #[test]
    fn parses_env_files() {
        let v = parse_env_file("A=1\nB=x=y\r\nJSON<<EOF\n{\n  \"a\": 1\n}\nEOF\n\nC=\n").unwrap();
        assert_eq!(
            v,
            vec![
                ("A".into(), "1".into()),
                ("B".into(), "x=y".into()),
                ("JSON".into(), "{\n  \"a\": 1\n}".into()),
                ("C".into(), "".into()),
            ]
        );
        assert!(parse_env_file("X<<EOF\nnever closed").is_err());
        assert!(parse_env_file("novalue").is_err());
        assert_eq!(
            parse_env_file("K<<ghadelimiter_1\na=b\nghadelimiter_1\n").unwrap(),
            vec![("K".into(), "a=b".into())]
        );
    }
}
