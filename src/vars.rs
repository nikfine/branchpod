//! `{{name}}` substitution for argument arrays and template files.
//!
//! Unknown names are an error, never an empty string: a silently empty argument is how teardown
//! ends up running `docker rm -f -v` with no id.
//!
//! `{{{{` is a literal `{{`, for tools with the same brackets (Go templates):
//! `{{{{.Config.Image}}` renders as `{{.Config.Image}}`. A `}}` outside a reference is literal as
//! it is, so it has no escape: doubling it would silently change text such as minified JSON.
//!
//! `{{name | filter}}` passes the value through filters, left to right: `json` escapes it for use
//! inside a JSON string (adding no quotes), `slash` writes a Windows path with `/`.

use std::borrow::Cow;
use std::collections::BTreeMap;

use anyhow::{Result, anyhow};

#[derive(Debug, Clone, Default)]
pub(crate) struct Vars {
    values: BTreeMap<String, String>,
    /// Known names without a value here, with the reason (`branch` on a detached HEAD). A
    /// reference to one is still an error, but one that says why.
    unavailable: BTreeMap<String, String>,
}

impl Vars {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        self.unavailable.remove(&name);
        self.values.insert(name, value.into());
    }

    /// Declares `name` as known but without a value, so that a reference to it explains `reason`.
    pub(crate) fn set_unavailable(&mut self, name: impl Into<String>, reason: impl Into<String>) {
        let name = name.into();
        self.values.remove(&name);
        self.unavailable.insert(name, reason.into());
    }

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Replaces every `{{ name }}` (surrounding spaces allowed) with its value. Errors quote the
    /// input, so this is meant for short strings such as arguments.
    pub(crate) fn render(&self, input: &str) -> Result<String> {
        self.render_inner(input)
            .map_err(|(_, problem)| anyhow!("{problem} in {input:?}"))
    }

    /// Like [`Vars::render`] for the contents of a file: errors name the line, not the text.
    pub(crate) fn render_text(&self, input: &str) -> Result<String> {
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
                Piece::Var {
                    offset,
                    name,
                    filters,
                } => {
                    let value = self
                        .value(name, &filters)
                        .map_err(|problem| (offset, problem))?;
                    out.push_str(&value);
                }
            }
        }
        Ok(out)
    }

    /// The value of `name` after `filters`. The variable is checked before the filters.
    fn value(&self, name: &str, filters: &[&str]) -> Result<Cow<'_, str>, String> {
        let Some(value) = self.get(name) else {
            return Err(self.missing(name, filters));
        };
        let mut value = Cow::Borrowed(value);
        for filter in filters {
            let Some(filter) = Filter::parse(filter) else {
                return Err(unknown_filter(filter));
            };
            value = filter
                .apply(&value)
                .map_err(|problem| format!("variable `{name}`: {problem}"))?
                .into();
        }
        Ok(value)
    }

    fn missing(&self, name: &str, filters: &[&str]) -> String {
        let foreign_filter = filters.iter().any(|filter| Filter::parse(filter).is_none());
        let hint = |foreign: bool| if foreign { ESCAPE_HINT } else { "" };
        if let Some(reason) = self.unavailable.get(name) {
            let hint = hint(foreign_filter);
            return format!("variable `{name}` is not available: {reason}{hint}");
        }
        let hint = hint(foreign_filter || looks_foreign(name));
        format!("unknown variable `{name}`{hint}")
    }

    pub(crate) fn render_all(&self, items: &[String]) -> Result<Vec<String>> {
        items.iter().map(|item| self.render(item)).collect()
    }
}

/// The names of the variables `input` references, with or without filters. Parsed exactly as
/// rendering does, so an escaped `{{{{name}}` is not a reference. Malformed references are ignored
/// here; rendering reports them.
pub(crate) fn names(input: &str) -> impl Iterator<Item = &str> {
    Pieces::new(input)
        .map_while(Result::ok)
        .filter_map(|piece| match piece {
            Piece::Text(_) => None,
            Piece::Var { name, .. } => Some(name),
        })
}

/// Whether `input` references the variable `name`, see [`names`].
pub(crate) fn references(input: &str, name: &str) -> bool {
    names(input).any(|n| n == name)
}

/// Appended to errors about text that is probably another tool's template.
const ESCAPE_HINT: &str = " (write `{{{{` for a literal `{{`)";

fn unknown_filter(filter: &str) -> String {
    if filter.is_empty() {
        format!("empty filter{ESCAPE_HINT}")
    } else {
        format!("unknown filter `{filter}`, expected `json` or `slash`{ESCAPE_HINT}")
    }
}

/// A conversion of a value. Filters take no arguments.
#[derive(Debug, Clone, Copy)]
enum Filter {
    Json,
    Slash,
}

impl Filter {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "json" => Some(Self::Json),
            "slash" => Some(Self::Slash),
            _ => None,
        }
    }

    fn apply(self, value: &str) -> Result<String, String> {
        match self {
            Self::Json => Ok(json(value)),
            Self::Slash => slash(value, cfg!(windows)),
        }
    }
}

/// Escapes `value` for use inside a JSON string. The surrounding quotes are not added, so that it
/// can be a part of one: `"{{worktree | json}}/src"`.
fn json(value: &str) -> String {
    let quoted = serde_json::Value::from(value).to_string();
    quoted[1..quoted.len() - 1].to_owned()
}

/// Writes a Windows path with `/`. On Unix `\` is an ordinary character of a file name, so nothing
/// changes there. Windows does not accept `/` in a `\\?\` path, so that is an error, except for
/// `\\?\UNC\server\share`, which has the plain form `//server/share`: it is how the path of a
/// worktree on a network share is spelled.
fn slash(value: &str, windows: bool) -> Result<String, String> {
    if !windows {
        return Ok(value.to_owned());
    }
    let (prefix, rest) = match verbatim_unc(value) {
        Some(rest) => ("//", rest),
        None => ("", value),
    };
    // The value is not quoted: template errors do not show content.
    if rest.contains(r"\\?\") {
        return Err(r"filter `slash`: a `\\?\` path cannot be written with `/`".to_owned());
    }
    Ok(format!("{prefix}{}", rest.replace('\\', "/")))
}

/// `server\share...` of a value that starts with `\\?\UNC\server\share`. A server named `?` or `.`
/// would turn into the prefix of another kind of path.
fn verbatim_unc(value: &str) -> Option<&str> {
    let rest = value.strip_prefix(r"\\?\UNC\")?;
    let mut parts = rest.split('\\');
    let server = parts.next().filter(|s| !matches!(*s, "" | "?" | "."));
    let share = parts.next().filter(|s| !s.is_empty());
    server.and(share).map(|_| rest)
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
    /// `offset` is the byte offset of the opening `{{`. `filters` are as written, unchecked.
    Var {
        offset: usize,
        name: &'a str,
        filters: Vec<&'a str>,
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
        let inner = &after[..end];
        if inner.trim().is_empty() {
            return self.fail(offset, "empty `{{}}`");
        }
        let mut parts = inner.split('|').map(str::trim);
        let name = parts.next().unwrap_or_default();
        if name.is_empty() {
            return self.fail(offset, &format!("no variable before `|`{ESCAPE_HINT}"));
        }
        self.pos += 2 + end + 2;
        Some(Ok(Piece::Var {
            offset,
            name,
            filters: parts.collect(),
        }))
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

    fn error(v: &Vars, input: &str) -> String {
        v.render(input).unwrap_err().to_string()
    }

    #[test]
    fn json_escapes_the_inside_of_a_string() {
        let mut v = vars();
        for (value, expected) in [
            (r"C:\work\feature-x", r"C:\\work\\feature-x"),
            (r#"say "hi""#, r#"say \"hi\""#),
            ("a\u{1}b\u{1f}c", r"a\u0001b\u001fc"),
            ("a\r\n\tb", r"a\r\n\tb"),
            ("путь/ü/日本", "путь/ü/日本"),
            ("", ""),
        ] {
            v.set("worktree", value);
            assert_eq!(v.render("{{worktree | json}}").unwrap(), expected);
            let document = v.render(r#"{"path":"{{worktree|json}}/src"}"#).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
            assert_eq!(parsed["path"], format!("{value}/src"));
        }
    }

    #[test]
    fn json_also_fits_a_toml_basic_string() {
        let value: String = ('\0'..='\u{1f}').chain("\"\\ü".chars()).collect();
        let document = format!("x = \"{}\"", json(&value));
        let parsed: toml::Table = document.parse().unwrap();
        assert_eq!(parsed["x"].as_str(), Some(value.as_str()));
        // The exception: TOML wants U+007F escaped, JSON does not.
        let document = format!("x = \"{}\"", json("\u{7f}"));
        assert!(document.parse::<toml::Table>().is_err());
    }

    #[test]
    fn slash_changes_windows_paths_only() {
        for (value, expected) in [
            (r"C:\work\feature-x", "C:/work/feature-x"),
            (r"C:\work/mixed\путь", "C:/work/mixed/путь"),
            (r"\\server\share\x", "//server/share/x"),
            ("/already", "/already"),
        ] {
            assert_eq!(slash(value, true).unwrap(), expected);
            assert_eq!(slash(value, false).unwrap(), value);
        }
        // A share has a plain form; any other `\\?\` path does not, wherever it is in the value.
        for (value, expected) in [
            (r"\\?\UNC\server\share", "//server/share"),
            (r"\\?\UNC\server\C$\путь\x", "//server/C$/путь/x"),
        ] {
            assert_eq!(slash(value, true).unwrap(), expected);
            assert_eq!(slash(value, false).unwrap(), value);
        }
        for verbatim in [
            r"\\?\C:\work",
            r"\\?\Volume{0}\x",
            r"C:\a;\\?\C:\b",
            r"C:\a;\\?\UNC\server\share",
            r"\\?\UNC\server\share;\\?\UNC\server\other",
            r"\\?\UNC\server",
            r"\\?\UNC\server\",
            r"\\?\UNC\\share",
            r"\\?\UNC\?\C:\x",
            r"\\?\UNC\.\pipe\x",
        ] {
            let err = slash(verbatim, true).unwrap_err();
            assert!(err.contains(r"`\\?\` path"), "{verbatim}: {err}");
            assert_eq!(slash(verbatim, false).unwrap(), verbatim);
        }
    }

    #[test]
    fn filters_run_left_to_right() {
        let mut v = vars();
        v.set("worktree", r#"C:\a "b""#);
        let (slash_json, json_slash) = if cfg!(windows) {
            (r#"C:/a \"b\""#, r#"C://a /"b/""#)
        } else {
            (r#"C:\\a \"b\""#, r#"C:\\a \"b\""#)
        };
        for input in ["{{worktree|slash|json}}", "{{ worktree | slash | json }}"] {
            assert_eq!(v.render(input).unwrap(), slash_json);
        }
        assert_eq!(v.render("{{worktree | json | slash}}").unwrap(), json_slash);
    }

    #[test]
    fn filters_next_to_escapes() {
        let mut v = vars();
        v.set("worktree", r"C:\a");
        assert_eq!(
            v.render("{{{{.X | json}} {{worktree | json}}").unwrap(),
            r"{{.X | json}} C:\\a"
        );
        assert_eq!(v.render("{{{{{{worktree | json}}}}").unwrap(), r"{{C:\\a}}");
        // A `|` outside a reference is text.
        assert_eq!(
            v.render("a | json {{port.web}} | b").unwrap(),
            "a | json 8003 | b"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_rejected_value_is_not_shown() {
        let mut v = vars();
        v.set("worktree", r"\\?\C:\secret");
        let err = v
            .render_text("a\n{{worktree | slash}}")
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            r"line 2: variable `worktree`: filter `slash`: a `\\?\` path cannot be written with `/`"
        );
    }

    #[test]
    fn unknown_and_empty_filters_are_errors() {
        let mut v = vars();
        v.set_unavailable("branch", "HEAD is detached");
        for (input, expected) in [
            ("{{port.web | upper}}", "unknown filter `upper`"),
            ("{{port.web | JSON}}", "unknown filter `JSON`"),
            ("{{port.web | json .}}", "unknown filter `json .`"),
            ("{{port.web | json | upper}}", "unknown filter `upper`"),
            ("{{port.web || 'x'}}", "empty filter"),
            ("{{port.web |}}", "empty filter"),
            ("{{| json}}", "no variable before `|`"),
        ] {
            let err = error(&v, input);
            assert!(err.starts_with(expected), "{err}");
            assert!(err.contains("for a literal `{{`"), "{err}");
        }
        // The variable comes first.
        let err = error(&v, "{{branch | upper}}");
        assert!(
            err.contains("HEAD is detached") && err.contains("for a literal `{{`"),
            "{err}"
        );
        let err = error(&v, "{{branch | json}}");
        assert!(
            err.contains("HEAD is detached") && !err.contains("literal"),
            "{err}"
        );
        let err = error(&v, "{{port.vite | json}}");
        assert!(err.starts_with("unknown variable `port.vite` in"), "{err}");
        let err = v.render_text("a\n{{port.web | upper}}").unwrap_err();
        assert!(
            err.to_string()
                .starts_with("line 2: unknown filter `upper`"),
            "{err}"
        );
    }

    #[test]
    fn foreign_pipelines_hint_at_the_escape() {
        for input in [
            "{{ .X | json }}",
            "{{.Values.name | quote}}",
            "{{ item.title | upper }}",
            "{{ item.title || 'none' }}",
            "{{ title | json }}",
        ] {
            let err = error(&vars(), input);
            assert!(
                err.starts_with("unknown variable") && err.contains("for a literal `{{`"),
                "{err}"
            );
        }
    }

    #[test]
    fn filtered_references_are_references() {
        let name = "command.app.stdout";
        for input in [
            "{{command.app.stdout | json}}",
            "x {{ command.app.stdout|slash|json }} y",
            "{{command.app.stdout | nope}}",
        ] {
            assert!(references(input, name), "{input}");
        }
        assert!(!references("{{{{command.app.stdout | json}}", name));
        assert!(!references("{{other | command.app.stdout}}", name));
        assert_eq!(
            names("{{a}} {{{{b}} {{ c | json }}").collect::<Vec<_>>(),
            ["a", "c"]
        );
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
