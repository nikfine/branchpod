//! `{{name}}` substitution for argument arrays and template files.
//!
//! Unknown names are an error, never an empty string: a silently empty argument is how teardown
//! ends up running `docker rm -f -v` with no id.
//!
//! `{{{{` is a literal `{{`, for tools with the same brackets (Go templates):
//! `{{{{.Config.Image}}` renders as `{{.Config.Image}}`. A `}}` outside a reference is literal as
//! it is, so it has no escape: doubling it would silently change text such as minified JSON.

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
        for piece in Pieces::new(input) {
            match piece? {
                Piece::Text(text) => out.push_str(text),
                Piece::Var { offset, name } => match self.get(name) {
                    Some(value) => out.push_str(value),
                    None => return Err((offset, self.missing(name))),
                },
            }
        }
        Ok(out)
    }

    fn missing(&self, name: &str) -> String {
        if let Some(reason) = self.unavailable.get(name) {
            return format!("variable `{name}` is not available: {reason}");
        }
        if looks_foreign(name) {
            format!("unknown variable `{name}` (write `{{{{{{{{` for a literal `{{{{`)")
        } else {
            format!("unknown variable `{name}`")
        }
    }

    pub fn render_all(&self, items: &[String]) -> Result<Vec<String>> {
        items.iter().map(|item| self.render(item)).collect()
    }
}

/// Whether `input` references the variable `name`. Parsed exactly as rendering does, so an
/// escaped `{{{{name}}` is not a reference. Malformed references are ignored here; rendering
/// reports them.
pub fn references(input: &str, name: &str) -> bool {
    Pieces::new(input)
        .map_while(Result::ok)
        .any(|piece| matches!(piece, Piece::Var { name: n, .. } if n == name))
}

/// Whether an undeclared `name` looks like a Go template action (`docker --format`,
/// `kubectl -o go-template`, Helm) rather than a misspelt `bp` variable. Every `bp` name without
/// a dot (`worktree`, `branch`, ...) is always declared, set or unavailable, so an undeclared one
/// without a dot (`end`, `else`) is not ours either.
fn looks_foreign(name: &str) -> bool {
    name.starts_with(['.', '$'])
        || !name.contains('.')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A piece of parsed input: literal text or a variable reference.
#[derive(Debug, PartialEq)]
enum Piece<'a> {
    Text(&'a str),
    /// `offset` is the byte offset of the opening `{{`.
    Var {
        offset: usize,
        name: &'a str,
    },
}

/// Splits input into [`Piece`]s, left to right. `{{{{` is a literal `{{`; `}}` outside a
/// reference is always literal and needs no escape. Yields at most one error, then stops.
struct Pieces<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Pieces<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn fail(&mut self, offset: usize, problem: &str) -> Option<Result<Piece<'a>, (usize, String)>> {
        self.pos = self.input.len();
        Some(Err((offset, problem.to_owned())))
    }
}

impl<'a> Iterator for Pieces<'a> {
    type Item = Result<Piece<'a>, (usize, String)>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = &self.input[self.pos..];
        if rest.is_empty() {
            return None;
        }
        let start = rest.find("{{").unwrap_or(rest.len());
        if start > 0 {
            self.pos += start;
            return Some(Ok(Piece::Text(&rest[..start])));
        }
        let offset = self.pos;
        let after = &rest[2..];
        if after.starts_with("{{") {
            self.pos += 4;
            return Some(Ok(Piece::Text(&rest[..2])));
        }
        let Some(end) = after.find("}}") else {
            return self.fail(offset, "unterminated `{{`");
        };
        let name = after[..end].trim();
        if name.is_empty() {
            return self.fail(offset, "empty `{{}}`");
        }
        self.pos += 2 + end + 2;
        Some(Ok(Piece::Var { offset, name }))
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
    fn escaped_references_are_not_references() {
        assert!(!references(
            "{{{{command.app.stdout}}",
            "command.app.stdout"
        ));
        assert!(references(
            "{{{{{{command.app.stdout}}}}",
            "command.app.stdout"
        ));
        assert!(references(
            "{{{{.Id}} {{command.app.stdout}}",
            "command.app.stdout"
        ));
    }

    #[test]
    fn quadruple_brace_is_a_literal() {
        let v = vars();
        assert_eq!(
            v.render("{{{{.Config.Image}}").unwrap(),
            "{{.Config.Image}}"
        );
        assert_eq!(
            v.render("{{{{range .Mounts}}{{{{.Source}} {{{{end}}")
                .unwrap(),
            "{{range .Mounts}}{{.Source}} {{end}}"
        );
        assert_eq!(
            v.render("{{{{json .}}-{{pod.id}}").unwrap(),
            "{{json .}}-feature-x-1a2b3c4d"
        );
        // Left to right: escape, reference, plain `}}`.
        assert_eq!(v.render("{{{{{{port.web}}}}").unwrap(), "{{8003}}");
        assert_eq!(v.render("{{{{").unwrap(), "{{");
        assert_eq!(v.render("a{{{{{{{{b").unwrap(), "a{{{{b");
    }

    #[test]
    fn closing_braces_are_left_alone() {
        let json = r#"{"a":{"b":{"c":{}}}}"#;
        assert_eq!(vars().render(json).unwrap(), json);
        assert_eq!(vars().render("}}}}{{port.web}}}}").unwrap(), "}}}}8003}}");
    }

    #[test]
    fn unknown_or_malformed_is_an_error() {
        assert!(vars().render("{{port.vite}}").is_err());
        assert!(vars().render("{{port.web").is_err());
        assert!(vars().render("{{}}").is_err());
        assert!(vars().render("{{{port.web}}}").is_err());
        assert!(vars().render("{{{{.Id}} {{port.vite}}").is_err());
    }

    #[test]
    fn go_template_errors_hint_at_the_escape() {
        let err = vars().render("{{.Config.Image}}").unwrap_err().to_string();
        assert!(err.contains("write `{{{{` for a literal `{{`"), "{err}");
        let err = vars().render("{{json .}}").unwrap_err().to_string();
        assert!(err.contains("literal"), "{err}");
        // An escaped `range` whose `end` was not escaped.
        let err = vars()
            .render("{{{{range .Mounts}}{{{{.Source}}{{end}}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`end` (write `{{{{`"), "{err}");
        // A declared name without a dot explains itself instead.
        let mut v = vars();
        v.set_unavailable("branch", "HEAD is detached");
        let err = v.render("{{branch}}").unwrap_err().to_string();
        assert!(!err.contains("literal"), "{err}");
        let err = vars().render("{{port.vite}}").unwrap_err().to_string();
        assert_eq!(err, r#"unknown variable `port.vite` in "{{port.vite}}""#);
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
        assert_eq!(
            vars()
                .render_text("a\n{{{{x}}\nb={{port.vite}}\n")
                .unwrap_err()
                .to_string(),
            "line 3: unknown variable `port.vite`"
        );
    }
}
