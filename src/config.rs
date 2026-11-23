//! Project configuration: `branchpod.toml` (committed) layered with `branchpod.local.toml`
//! (git-ignored, per user).
//!
//! The project file is read from the worktree, so every branch carries its own version. The local
//! file is read from the main checkout, because git-ignored files do not exist in a fresh worktree.
//!
//! Layering: tables are merged key by key, every other value (including arrays such as `[[file]]`)
//! is replaced as a whole by the local file.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const PROJECT_FILE: &str = "branchpod.toml";
pub const LOCAL_FILE: &str = "branchpod.local.toml";
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Config schema version. Required, so that a future format change is detected, not guessed.
    pub version: u32,

    /// Named ports, referenced as `{{port.<name>}}`.
    #[serde(default)]
    pub port: std::collections::BTreeMap<String, PortSpec>,

    /// Git-ignored files carried over from the main checkout.
    #[serde(default)]
    pub file: Vec<FileSpec>,

    /// External resources managed by create/destroy commands (containers, volumes, sync sessions).
    #[serde(default)]
    pub command: Vec<CommandSpec>,

    /// Long-running processes under the supervisor.
    #[serde(default)]
    pub service: Vec<ServiceSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortSpec {
    /// Inclusive range `[first, last]` to allocate from.
    pub range: [u16; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileMode {
    /// Plain copy.
    Copy,
    /// Copy-on-write clone where the filesystem supports it, plain copy otherwise.
    Reflink,
    /// Symlink (junction for directories on Windows) pointing into the main checkout.
    Link,
    /// Copy with `{{var}}` substitution and literal `replace` rules applied.
    Template,
    /// Known and intentionally not carried over; silences `bp doctor`.
    Ignore,
}

impl FileMode {
    /// The name used in the config.
    pub fn as_str(self) -> &'static str {
        match self {
            FileMode::Copy => "copy",
            FileMode::Reflink => "reflink",
            FileMode::Link => "link",
            FileMode::Template => "template",
            FileMode::Ignore => "ignore",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    /// Destination, relative to the worktree root.
    pub path: String,
    pub mode: FileMode,
    /// Source, relative to the main checkout. Defaults to `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// A missing source is skipped instead of failing `bp up`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    /// Template mode: substitute `{{var}}` in the source. `false` for sources that legitimately
    /// contain `{{ }}` (Blade, Vue); only `replace` rules are applied then.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub vars: bool,
    /// Literal find/replace rules, applied after `{{var}}` substitution. Template mode only.
    /// A rule that matches nothing is an error, so drift in the source file is noticed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replace: Vec<Replace>,
}

fn yes() -> bool {
    true
}

fn is_yes(value: &bool) -> bool {
    *value
}

impl FileSpec {
    /// Source path relative to the main checkout.
    pub fn source(&self) -> &str {
        self.source.as_deref().unwrap_or(&self.path)
    }

    /// Identity of the resource in the ledger: the normalized destination path.
    pub fn name(&self) -> String {
        normalize(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replace {
    pub find: String,
    /// Replacement; may contain `{{var}}` references.
    pub with: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    /// Referenced as `{{command.<name>.stdout}}` by later resources.
    pub name: String,
    /// Argument array; executed without a shell.
    pub create: Vec<String>,
    /// Argument array run on teardown; may reference this command's captured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destroy: Option<Vec<String>>,
    /// Argument array listing leftovers by label, one id per line; used by `bp gc`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    pub name: String,
    /// Argument array; executed without a shell.
    pub run: Vec<String>,
    /// Working directory relative to the worktree root. Defaults to the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
}

impl Config {
    /// Loads the project file from `worktree` and layers the local file from `main_checkout` on top.
    pub fn load(worktree: &Path, main_checkout: Option<&Path>) -> Result<Config> {
        let project_path = worktree.join(PROJECT_FILE);
        let mut value = read_table(&project_path)?
            .with_context(|| format!("no {PROJECT_FILE} in {}", worktree.display()))?;

        if let Some(main) = main_checkout
            && let Some(local) = read_table(&main.join(LOCAL_FILE))?
        {
            merge(&mut value, local);
        }

        let config: Config = value
            .try_into()
            .with_context(|| format!("invalid configuration in {}", project_path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn parse(text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != SCHEMA_VERSION {
            bail!(
                "unsupported config version {} (this build supports {SCHEMA_VERSION})",
                self.version
            );
        }

        for (name, port) in &self.port {
            check_name("port", name)?;
            let [first, last] = port.range;
            if first == 0 || first > last {
                bail!("port.{name}: range must be [first, last] with 0 < first <= last");
            }
        }

        let mut paths = BTreeSet::new();
        for file in &self.file {
            check_relative(&file.path).with_context(|| format!("file {:?}", file.path))?;
            if let Some(source) = &file.source {
                check_relative(source).with_context(|| format!("file {:?}: source", file.path))?;
                if normalize(source).is_empty() {
                    bail!(
                        "file {:?}: source must name something inside the main checkout",
                        file.path
                    );
                }
            }
            if !file.replace.is_empty() && file.mode != FileMode::Template {
                bail!(
                    "file {:?}: `replace` is only allowed with mode = \"template\"",
                    file.path
                );
            }
            if file.replace.iter().any(|r| r.find.is_empty()) {
                bail!("file {:?}: `replace.find` must not be empty", file.path);
            }
            if !file.vars && file.mode != FileMode::Template {
                bail!(
                    "file {:?}: `vars` is only allowed with mode = \"template\"",
                    file.path
                );
            }
            if !file.vars && file.replace.is_empty() {
                bail!(
                    "file {:?}: a template with `vars = false` and no `replace` rules is a \
                     plain copy; use mode = \"copy\"",
                    file.path
                );
            }
            if file.mode == FileMode::Ignore && (file.optional || file.source.is_some()) {
                bail!(
                    "file {:?}: `optional` and `source` have no effect with mode = \"ignore\"",
                    file.path
                );
            }
            if normalize(&file.path).is_empty() {
                bail!(
                    "file {:?}: path must name something inside the worktree",
                    file.path
                );
            }
            // Case-insensitive: on Windows and macOS `.env` and `.ENV` are the same file, and the
            // same config must mean the same thing on every platform.
            if !paths.insert(normalize(&file.path).to_lowercase()) {
                bail!("file {:?} is listed twice", file.path);
            }
        }

        let mut names = BTreeSet::new();
        for command in &self.command {
            check_name("command", &command.name)?;
            if !names.insert(command.name.as_str()) {
                bail!("command {:?} is defined twice", command.name);
            }
            check_argv(&command.create)
                .with_context(|| format!("command.{}.create", command.name))?;
            for (field, argv) in [("destroy", &command.destroy), ("gc", &command.gc)] {
                if let Some(argv) = argv {
                    check_argv(argv)
                        .with_context(|| format!("command.{}.{field}", command.name))?;
                }
            }
        }

        let mut names = BTreeSet::new();
        for service in &self.service {
            check_name("service", &service.name)?;
            if !names.insert(service.name.as_str()) {
                bail!("service {:?} is defined twice", service.name);
            }
            check_argv(&service.run).with_context(|| format!("service.{}.run", service.name))?;
            if let Some(cwd) = &service.cwd {
                check_relative(cwd).with_context(|| format!("service.{}.cwd", service.name))?;
            }
        }

        Ok(())
    }
}

fn read_table(path: &Path) -> Result<Option<toml::Table>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let table = text
        .parse::<toml::Table>()
        .with_context(|| format!("cannot parse {}", path.display()))?;
    Ok(Some(table))
}

/// Merges `overlay` into `base`: tables recursively, everything else replaced.
fn merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Names become variable segments (`port.web`) and labels, so keep them boring.
fn check_name(kind: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !ok {
        bail!("{kind} name {name:?} must match [a-z][a-z0-9_-]* (at most 64 characters)");
    }
    Ok(())
}

fn check_argv(argv: &[String]) -> Result<()> {
    match argv.first() {
        None => bail!("argument array must not be empty"),
        Some(program) if program.trim().is_empty() => bail!("program name must not be empty"),
        Some(_) => Ok(()),
    }
}

/// Paths in config are relative to the worktree or the main checkout and must stay inside it.
fn check_relative(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("path must not be empty");
    }
    // Checked textually as well: on Unix `C:\x` is a valid relative file name, but the same
    // config must mean the same thing on every platform.
    let looks_absolute =
        path.starts_with('/') || path.starts_with('\\') || path.as_bytes().get(1) == Some(&b':');
    if looks_absolute {
        bail!("path must be relative");
    }
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => bail!("path must not contain `..`"),
            Component::RootDir | Component::Prefix(_) => bail!("path must be relative"),
        }
    }
    if path.split(['/', '\\']).any(|segment| segment == "..") {
        bail!("path must not contain `..`");
    }
    // A drive letter in the middle (`a/C:/b`) makes Windows discard everything before it, and
    // `name:stream` writes an NTFS alternate data stream. `:` is not valid in Windows names.
    if path.contains(':') {
        bail!("path must not contain `:`");
    }
    Ok(())
}

fn normalize(path: &str) -> String {
    path.split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
        version = 1

        [port.web]
        range = [8001, 8020]

        [[file]]
        path = ".env"
        mode = "template"
        replace = [{ find = "localhost:8000", with = "localhost:{{port.web}}" }]

        [[file]]
        path = "node_modules"
        mode = "link"

        [[command]]
        name = "app"
        create = ["docker", "run", "-d", "--label", "bp.pod={{pod.id}}", "example/app"]
        destroy = ["docker", "rm", "-f", "-v", "{{command.app.stdout}}"]

        [[service]]
        name = "vite"
        run = ["npx", "vite", "--port", "{{port.web}}"]
        env = { NODE_ENV = "development" }
    "#;

    #[test]
    fn parses_example() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert_eq!(config.port["web"].range, [8001, 8020]);
        assert_eq!(config.file[0].mode, FileMode::Template);
        assert_eq!(config.command[0].name, "app");
        assert_eq!(config.service[0].env["NODE_ENV"], "development");
    }

    #[test]
    fn mode_names_match_the_config() {
        use FileMode::*;
        for mode in [Copy, Reflink, Link, Template, Ignore] {
            assert_eq!(serde_json::to_value(mode).unwrap(), mode.as_str());
        }
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = Config::parse("version = 1\n[[file]]\npath = 'a'\nmode = 'copy'\nsorce = 'b'\n");
        assert!(err.is_err());
    }

    #[test]
    fn requires_version() {
        assert!(Config::parse("").is_err());
        assert!(Config::parse("version = 2").is_err());
    }

    #[test]
    fn rejects_paths_escaping_the_tree() {
        for path in [
            "../x",
            "a/../../x",
            "/etc/x",
            "C:\\x",
            "\\\\server\\share",
            "a\\..\\..\\x",
            "x/C:/y",
            "x\\C:y",
            ".env:stream",
        ] {
            let text = format!("version = 1\n[[file]]\npath = '{path}'\nmode = 'copy'\n");
            assert!(Config::parse(&text).is_err(), "{path} should be rejected");
            let text =
                format!("version = 1\n[[file]]\npath = 'a'\nsource = '{path}'\nmode = 'copy'\n");
            assert!(
                Config::parse(&text).is_err(),
                "source {path} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_replace_outside_template_mode() {
        let text = "version = 1\n[[file]]\npath = 'a'\nmode = 'copy'\nreplace = [{ find = 'x', with = 'y' }]\n";
        assert!(Config::parse(text).is_err());
    }

    #[test]
    fn template_options() {
        let base = "version = 1\n[[file]]\npath = 'a.blade.php'\n";
        let ok = format!(
            "{base}mode = 'template'\nvars = false\nreplace = [{{ find = 'x', with = 'y' }}]\noptional = true\n"
        );
        let config = Config::parse(&ok).unwrap();
        assert!(!config.file[0].vars);
        assert!(config.file[0].optional);
        // Defaults are left out when printed back.
        let printed = toml::to_string(&Config::parse(EXAMPLE).unwrap()).unwrap();
        assert!(
            !printed.contains("vars") && !printed.contains("optional"),
            "{printed}"
        );

        for bad in [
            format!("{base}mode = 'copy'\nvars = false\n"),
            format!("{base}mode = 'template'\nvars = false\n"),
            format!("{base}mode = 'ignore'\noptional = true\n"),
            "version = 1\n[[file]]\npath = './'\nmode = 'copy'\n".to_owned(),
            "version = 1\n[[file]]\npath = 'all'\nsource = '.'\nmode = 'copy'\n".to_owned(),
        ] {
            assert!(Config::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_duplicates_and_bad_names() {
        let dup = "version = 1\n[[service]]\nname = 'a'\nrun = ['x']\n[[service]]\nname = 'a'\nrun = ['y']\n";
        assert!(Config::parse(dup).is_err());
        let dup_file = "version = 1\n[[file]]\npath = 'a/b'\nmode = 'copy'\n[[file]]\npath = './a\\\\b'\nmode = 'link'\n";
        assert!(Config::parse(dup_file).is_err());
        let dup_case = "version = 1\n[[file]]\npath = '.env'\nmode = 'copy'\n[[file]]\npath = '.ENV'\nmode = 'copy'\n";
        assert!(Config::parse(dup_case).is_err());
        assert!(Config::parse("version = 1\n[port.Web]\nrange = [1, 2]\n").is_err());
        assert!(Config::parse("version = 1\n[port.web]\nrange = [9, 2]\n").is_err());
        assert!(Config::parse("version = 1\n[[service]]\nname = 's'\nrun = []\n").is_err());
    }

    #[test]
    fn local_file_overrides_tables_and_replaces_arrays() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join("wt");
        let main = dir.path().join("main");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&main).unwrap();
        fs::write(worktree.join(PROJECT_FILE), EXAMPLE).unwrap();
        fs::write(
            main.join(LOCAL_FILE),
            "[port.web]\nrange = [9001, 9005]\n[[file]]\npath = '.env'\nmode = 'copy'\n",
        )
        .unwrap();

        let config = Config::load(&worktree, Some(&main)).unwrap();
        assert_eq!(config.port["web"].range, [9001, 9005]);
        assert_eq!(config.file.len(), 1);
        assert_eq!(config.file[0].mode, FileMode::Copy);
        assert_eq!(config.service.len(), 1);
    }

    #[test]
    fn missing_project_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = Config::load(dir.path(), None).unwrap_err();
        assert!(format!("{err:#}").contains(PROJECT_FILE));
    }
}
