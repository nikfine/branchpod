//! `{{name}}` substitution for argument arrays and template files.
//!
//! Unknown names are an error, never an empty string: a silently empty argument is how teardown
//! ends up running `docker rm -f -v` with no id.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

#[derive(Debug, Clone, Default)]
pub struct Vars(BTreeMap<String, String>);

impl Vars {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.0.insert(name.into(), value.into());
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Replaces every `{{ name }}` (surrounding spaces allowed) with its value.
    pub fn render(&self, input: &str) -> Result<String> {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                bail!("unterminated `{{{{` in {input:?}");
            };
            let name = after[..end].trim();
            match self.get(name) {
                Some(value) => out.push_str(value),
                None if name.is_empty() => bail!("empty `{{{{}}}}` in {input:?}"),
                None => bail!("unknown variable `{name}` in {input:?}"),
            }
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }

    pub fn render_all(&self, items: &[String]) -> Result<Vec<String>> {
        items.iter().map(|item| self.render(item)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        let mut v = Vars::new();
        v.set("port.web", "8003");
        v.set("pod.id", "feature-x-1a2b3c4d");
        v
    }

    #[test]
    fn renders_known_names() {
        assert_eq!(
            vars().render("http://localhost:{{port.web}}/").unwrap(),
            "http://localhost:8003/"
        );
        assert_eq!(
            vars().render("{{ pod.id }}-{{port.web}}").unwrap(),
            "feature-x-1a2b3c4d-8003"
        );
        assert_eq!(vars().render("no vars").unwrap(), "no vars");
    }

    #[test]
    fn values_are_not_rendered_again() {
        let mut v = Vars::new();
        v.set("a", "{{b}}");
        assert_eq!(v.render("{{a}}").unwrap(), "{{b}}");
    }

    #[test]
    fn unknown_or_malformed_is_an_error() {
        assert!(vars().render("{{port.vite}}").is_err());
        assert!(vars().render("{{port.web").is_err());
        assert!(vars().render("{{}}").is_err());
    }
}
