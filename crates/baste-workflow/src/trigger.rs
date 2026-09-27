//! The `on:` block: which events start a workflow and with which filters.

use crate::filter::PatternList;
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefFilter {
    pub branches: Option<Vec<String>>,
    pub branches_ignore: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub tags_ignore: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub paths_ignore: Option<Vec<String>>,
    /// `pull_request` activity types.
    pub types: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Triggers {
    pub push: Option<RefFilter>,
    pub pull_request: Option<RefFilter>,
    /// Every event name listed, including push and pull_request.
    pub events: Vec<String>,
}

impl Triggers {
    pub fn parse(v: &Value) -> Result<Triggers, String> {
        let mut t = Triggers::default();
        match v {
            Value::String(s) => t.add(s, &Value::Null)?,
            Value::Array(items) => {
                for item in items {
                    let name = item.as_str().ok_or("'on' list entries must be strings")?;
                    t.add(name, &Value::Null)?;
                }
            }
            Value::Object(map) => {
                for (name, cfg) in map {
                    t.add(name, cfg)?;
                }
            }
            _ => return Err("'on' must be a string, list or mapping".into()),
        }
        Ok(t)
    }

    fn add(&mut self, event: &str, cfg: &Value) -> Result<(), String> {
        self.events.push(event.to_string());
        match event {
            "push" => self.push = Some(RefFilter::parse(cfg)?),
            "pull_request" => self.pull_request = Some(RefFilter::parse(cfg)?),
            _ => {}
        }
        Ok(())
    }

    /// Whether a push of `git_ref` (`refs/heads/x` or `refs/tags/x`) starts
    /// the workflow. `changed` is `None` when the changed files are unknown,
    /// in which case path filters pass.
    pub fn matches_push(&self, git_ref: &str, changed: Option<&[String]>) -> Result<bool, String> {
        let Some(f) = &self.push else {
            return Ok(false);
        };
        let has_branch_filter = f.branches.is_some() || f.branches_ignore.is_some();
        let has_tag_filter = f.tags.is_some() || f.tags_ignore.is_some();
        if let Some(tag) = git_ref.strip_prefix("refs/tags/") {
            if has_branch_filter && !has_tag_filter {
                return Ok(false);
            }
            // Path filters are not evaluated for tag pushes.
            return name_passes(tag, &f.tags, &f.tags_ignore);
        }
        let branch = git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref);
        if has_tag_filter && !has_branch_filter {
            return Ok(false);
        }
        Ok(name_passes(branch, &f.branches, &f.branches_ignore)? && paths_pass(f, changed)?)
    }

    /// Whether a `pull_request` event of `action` (e.g. `synchronize`) against
    /// `base_branch` starts the workflow.
    pub fn matches_pull_request(
        &self,
        base_branch: &str,
        action: &str,
        changed: Option<&[String]>,
    ) -> Result<bool, String> {
        let Some(f) = &self.pull_request else {
            return Ok(false);
        };
        let types = f
            .types
            .clone()
            .unwrap_or_else(|| vec!["opened".into(), "synchronize".into(), "reopened".into()]);
        if !types.iter().any(|t| t == action) {
            return Ok(false);
        }
        Ok(name_passes(base_branch, &f.branches, &f.branches_ignore)? && paths_pass(f, changed)?)
    }
}

impl RefFilter {
    fn parse(v: &Value) -> Result<RefFilter, String> {
        let obj = match v {
            Value::Null => return Ok(RefFilter::default()),
            Value::Object(o) => o,
            _ => return Err("event configuration must be a mapping".into()),
        };
        let list = |key: &str| -> Result<Option<Vec<String>>, String> {
            match obj.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(vec![s.clone()])),
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|x| {
                        crate::model::scalar_string(x)
                            .ok_or_else(|| format!("'{key}' entries must be strings"))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(Some),
                Some(_) => Err(format!("'{key}' must be a string or a list")),
            }
        };
        let f = RefFilter {
            branches: list("branches")?,
            branches_ignore: list("branches-ignore")?,
            tags: list("tags")?,
            tags_ignore: list("tags-ignore")?,
            paths: list("paths")?,
            paths_ignore: list("paths-ignore")?,
            types: list("types")?,
        };
        if f.branches.is_some() && f.branches_ignore.is_some() {
            return Err("can't use both 'branches' and 'branches-ignore'".into());
        }
        if f.tags.is_some() && f.tags_ignore.is_some() {
            return Err("can't use both 'tags' and 'tags-ignore'".into());
        }
        if f.paths.is_some() && f.paths_ignore.is_some() {
            return Err("can't use both 'paths' and 'paths-ignore'".into());
        }
        Ok(f)
    }
}

fn name_passes(
    name: &str,
    include: &Option<Vec<String>>,
    ignore: &Option<Vec<String>>,
) -> Result<bool, String> {
    if let Some(patterns) = include {
        return Ok(PatternList::new(patterns)?.includes(name));
    }
    if let Some(patterns) = ignore {
        return Ok(!PatternList::new(patterns)?.includes(name));
    }
    Ok(true)
}

fn paths_pass(f: &RefFilter, changed: Option<&[String]>) -> Result<bool, String> {
    let Some(changed) = changed else {
        return Ok(true);
    };
    if let Some(patterns) = &f.paths {
        let list = PatternList::new(patterns)?;
        return Ok(changed.iter().any(|p| list.includes(p)));
    }
    if let Some(patterns) = &f.paths_ignore {
        let list = PatternList::new(patterns)?;
        return Ok(changed.iter().any(|p| !list.includes(p)));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml;

    fn t(src: &str) -> Triggers {
        Triggers::parse(&yaml::parse(src).unwrap()["on"]).unwrap()
    }

    fn files(f: &[&str]) -> Vec<String> {
        f.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn simple_forms() {
        assert!(t("on: push").matches_push("refs/heads/x", None).unwrap());
        assert!(t("on: [push, pull_request]")
            .matches_pull_request("main", "synchronize", None)
            .unwrap());
        assert!(!t("on: workflow_dispatch")
            .matches_push("refs/heads/x", None)
            .unwrap());
        assert!(t("on: push").matches_push("refs/tags/v1", None).unwrap());
    }

    #[test]
    fn branch_and_tag_filters() {
        let tr = t("on:\n  push:\n    branches: [main, 'releases/**']\n");
        assert!(tr.matches_push("refs/heads/main", None).unwrap());
        assert!(tr.matches_push("refs/heads/releases/1.0", None).unwrap());
        assert!(!tr.matches_push("refs/heads/feature", None).unwrap());
        assert!(
            !tr.matches_push("refs/tags/v1", None).unwrap(),
            "tags don't run when only branches are filtered"
        );

        let tr = t("on:\n  push:\n    tags: ['v*']\n");
        assert!(tr.matches_push("refs/tags/v1.0", None).unwrap());
        assert!(!tr.matches_push("refs/heads/main", None).unwrap());

        let tr = t("on:\n  push:\n    branches-ignore: ['dependabot/**']\n");
        assert!(tr.matches_push("refs/heads/main", None).unwrap());
        assert!(!tr
            .matches_push("refs/heads/dependabot/npm/x", None)
            .unwrap());
    }

    #[test]
    fn path_filters() {
        let tr = t("on:\n  push:\n    paths: ['src/**', '!src/**/*.md']\n");
        assert!(tr
            .matches_push("refs/heads/x", Some(&files(&["src/a.rs"])))
            .unwrap());
        assert!(!tr
            .matches_push("refs/heads/x", Some(&files(&["src/a.md"])))
            .unwrap());
        assert!(!tr
            .matches_push("refs/heads/x", Some(&files(&["docs/a"])))
            .unwrap());
        assert!(tr.matches_push("refs/heads/x", None).unwrap());
        assert!(tr
            .matches_push("refs/tags/v1", Some(&files(&["docs/a"])))
            .unwrap());

        let tr = t("on:\n  push:\n    paths-ignore: ['docs/**']\n");
        assert!(!tr
            .matches_push("refs/heads/x", Some(&files(&["docs/a"])))
            .unwrap());
        assert!(tr
            .matches_push("refs/heads/x", Some(&files(&["docs/a", "src/b"])))
            .unwrap());
    }

    #[test]
    fn pull_request_filters() {
        let tr = t("on:\n  pull_request:\n    branches: [main]\n");
        assert!(tr
            .matches_pull_request("main", "synchronize", None)
            .unwrap());
        assert!(!tr
            .matches_pull_request("develop", "synchronize", None)
            .unwrap());
        assert!(!tr.matches_pull_request("main", "closed", None).unwrap());
        let tr = t("on:\n  pull_request:\n    types: [closed]\n");
        assert!(!tr
            .matches_pull_request("main", "synchronize", None)
            .unwrap());
    }

    #[test]
    fn conflicting_filters_error() {
        let v = yaml::parse("on:\n  push:\n    branches: [a]\n    branches-ignore: [b]\n").unwrap();
        assert!(Triggers::parse(&v["on"]).is_err());
    }
}
