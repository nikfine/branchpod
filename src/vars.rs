//! `{{name}}` substitution for argument arrays and template files.
//!
//! Unknown names are an error, never an empty string: a silently empty argument is how teardown
//! ends up running `docker rm -f -v` with no id.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};

#[derive(Debug, Clone, Default)]
pub struct Vars {
    values: BTreeMap<String, String>,
    /// Known names without a value here, with the reason (`branch` on a detached HEAD). A
    /// reference to one is still an error, but one that says why.
    unavailable: BTreeMap<String, String>,
}

impl Vars {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        self.unavailable.remove(&name);
        self.values.insert(name, value.into());
    }

    /// Declares `name` as known but without a value, so that a reference to it explains `reason`.
    pub fn set_unavailable(&mut self, name: impl Into<String>, reason: impl Into<String>) {
        let name = name.into();
        self.values.remove(&name);
        self.unavailable.insert(name, reason.into());
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Replaces every `{{ name }}` (surrounding spaces allowed) with its value. Errors quote the
    /// input, so this is meant for short strings such as arguments.
    pub fn render(&self, input: &str) -> Result<String> {
        self.render_inner(input)
            .map_err(|(_, problem)| anyhow!("{problem} in {input:?}"))
    }

    /// Like [`Vars::render`] for the contents of a file: errors name the line, not the text.
    pub fn render_text(&self, input: &str) -> Result<String> {
        self.render_inner(input).map_err(|(offset, problem)| {
            let line = input[..offset].matches('\n').count() + 1;
            anyhow!("line {line}: {problem}")
        })
    }

    /// On error returns the byte offset of the offending `{{` and what is wrong with it.
    fn render_inner(&self, input: &str) -> Result<String, (usize, String)> {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(start) = rest.find("{{") {
            let offset = input.len() - rest.len() + start;
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                return Err((offset, "unterminated `{{`".to_owned()));
            };
            let name = after[..end].trim();
            match self.get(name) {
                Some(value) => out.push_str(value),
                None if name.is_empty() => return Err((offset, "empty `{{}}`".to_owned())),
                None => {
                    let problem = match self.unavailable.get(name) {
                        Some(reason) => format!("variable `{name}` is not available: {reason}"),
                        None => format!("unknown variable `{name}`"),
                    };
                    return Err((offset, problem));
                }
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

/// Whether `input` references the variable `name`. Malformed references are ignored here;
/// rendering reports them.
pub fn references(input: &str, name: &str) -> bool {
    let mut rest = input;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return false;
        };
        if after[..end].trim() == name {
            return true;
        }
        rest = &after[end + 2..];
    }
    false
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
    fn finds_references() {
        assert!(references(
            "x {{ command.app.stdout }} y",
            "command.app.stdout"
        ));
        assert!(references("{{a}}{{b}}", "b"));
        assert!(!references("{{command.app.stdout2}}", "command.app.stdout"));
        assert!(!references("command.app.stdout", "command.app.stdout"));
        assert!(!references("{{command.app.stdout", "command.app.stdout"));
    }

    #[test]
    fn unknown_or_malformed_is_an_error() {
        assert!(vars().render("{{port.vite}}").is_err());
        assert!(vars().render("{{port.web").is_err());
        assert!(vars().render("{{}}").is_err());
    }

    #[test]
    fn unavailable_names_explain_why() {
        let mut v = vars();
        v.set_unavailable("branch", "HEAD is detached");
        let err = v.render("{{branch}}").unwrap_err().to_string();
        assert!(
            err.contains("`branch` is not available: HEAD is detached"),
            "{err}"
        );
        v.set("branch", "main");
        assert_eq!(v.render("{{branch}}").unwrap(), "main");
    }

    #[test]
    fn text_errors_name_the_line_not_the_content() {
        let text = "secret=1\nurl=http://localhost:{{port.web}}\nb={{port.vite}}\n";
        let err = vars().render_text(text).unwrap_err().to_string();
        assert_eq!(err, "line 3: unknown variable `port.vite`");
        assert!(
            vars()
                .render_text("a\r\n{{x")
                .unwrap_err()
                .to_string()
                .starts_with("line 2")
        );
        assert_eq!(
            vars().render_text("\u{feff}a\r\n{{port.web}}\r\n").unwrap(),
            "\u{feff}a\r\n8003\r\n"
        );
    }
}
