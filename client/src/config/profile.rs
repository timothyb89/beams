//! Strict, workstation-side profile parsing and inheritance. Source paths keep
//! their declaring profile; named definitions replace rather than deep-merge.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
pub use beams_protocol::launch::{FileMode, Overwrite, Phase, Rerun, Shell};
use beams_protocol::ManifestParam;
use serde::{Deserialize, Serialize};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Packages {
    #[serde(default)]
    pre: PackageEdits,
    #[serde(default)]
    post: PackageEdits,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageEdits {
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}

fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    #[serde(default = "enabled")]
    pub enabled: bool,
    pub source: Option<String>,
    pub target: Option<String>,
    #[serde(default)]
    pub phase: Phase,
    #[serde(default)]
    pub mode: FileMode,
    #[serde(default)]
    pub overwrite: Overwrite,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub after: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    #[serde(default = "enabled")]
    pub enabled: bool,
    pub label: Option<String>,
    #[serde(default)]
    pub phase: Phase,
    pub script: Option<String>,
    pub script_root: Option<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    pub interpreter: Option<Vec<String>>,
    #[serde(default)]
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub check: Option<Vec<String>>,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    pub template: Option<String>,
    #[serde(default)]
    pub params: BTreeMap<String, ManifestParam>,
    #[serde(default)]
    pub rerun: Rerun,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    pub kind: String,
    pub default: Option<ManifestParam>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptTemplate {
    #[serde(default = "enabled")]
    pub enabled: bool,
    pub script: Option<String>,
    pub script_root: Option<String>,
    pub label: Option<String>,
    pub interpreter: Option<Vec<String>>,
    #[serde(default)]
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub check: Option<Vec<String>>,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub params: BTreeMap<String, Parameter>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    schema_version: u32,
    #[serde(default)]
    extends: Vec<String>,
    #[serde(default)]
    shell: Shell,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    packages: Packages,
    #[serde(default)]
    files: BTreeMap<String, File>,
    #[serde(default)]
    tasks: BTreeMap<String, Task>,
    #[serde(default)]
    templates: BTreeMap<String, ScriptTemplate>,
    ssh_grant: Option<SshGrant>,
}

/// `[ssh_grant]`: lets this launch's SSH tasks use the workstation's key
/// without a prompt, within limits the workstation enforces (see
/// `startup::GrantRecord`). A profile's section replaces an inherited one
/// whole; `hosts = []` removes it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshGrant {
    /// Host names as the workstation's known_hosts spells them.
    pub hosts: Vec<String>,
    /// Default `git`.
    pub user: Option<String>,
    /// From when the covered tasks' phase starts. Default 600, max 3600.
    pub window_secs: Option<u64>,
    /// Default: one per covered task, plus two.
    pub max_uses: Option<u32>,
    /// Task IDs it covers. Default: every `clone-git-repo` task with an
    /// SSH-style `repo` (`git@…`, `ssh://…`).
    pub tasks: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Origin {
    pub profile: String,
    pub key: String,
    #[serde(skip)]
    pub directory: Option<PathBuf>,
}

impl Origin {
    pub fn at(&self, key: &str) -> Self {
        Self {
            key: key.to_owned(),
            ..self.clone()
        }
    }
    pub fn context(&self) -> String {
        format!("{}: {}", self.profile, self.key)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "path", rename_all = "lowercase")]
pub enum Source {
    Local(PathBuf),
    Embedded(String),
}

pub fn embedded_asset(name: &str) -> Option<&'static [u8]> {
    match name {
        "assets/config.fish" => Some(include_bytes!("../../../environments/assets/config.fish")),
        "assets/starship.toml" => {
            Some(include_bytes!("../../../environments/assets/starship.toml"))
        }
        _ => None,
    }
}

impl Origin {
    /// Metadata-only: declared roots are checked, never recursively walked here.
    pub fn source(&self, input: &str) -> anyhow::Result<Source> {
        if let Some(directory) = &self.directory {
            let path = local_path(input, directory)?;
            Ok(Source::Local(path.canonicalize().with_context(|| {
                format!("{}: missing source {}", self.context(), path.display())
            })?))
        } else {
            // Bundled profiles use explicit assets. No dependency on a source checkout.
            if embedded_asset(input).is_some() {
                Ok(Source::Embedded(input.to_owned()))
            } else {
                bail!("{}: unknown bundled source {input:?}", self.context());
            }
        }
    }
}

pub fn local_path(input: &str, directory: &Path) -> anyhow::Result<PathBuf> {
    if input == "~" || input.starts_with("~/") {
        let home = std::env::var_os("HOME").context("HOME is needed for a local ~ path")?;
        return Ok(PathBuf::from(home).join(input.strip_prefix("~/").unwrap_or("")));
    }
    let path = PathBuf::from(input);
    Ok(if path.is_absolute() {
        path
    } else {
        directory.join(path)
    })
}

#[derive(Clone)]
pub struct Declared<T> {
    pub value: T,
    pub origin: Origin,
}

#[derive(Default)]
pub struct Resolved {
    pub profiles: Vec<String>,
    pub shell: Shell,
    pub env: BTreeMap<String, String>,
    pub origins: BTreeMap<String, Origin>,
    pub packages: BTreeMap<Phase, Vec<Declared<String>>>,
    pub removed_packages: BTreeMap<Phase, Vec<String>>,
    pub files: BTreeMap<String, Declared<File>>,
    pub tasks: BTreeMap<String, Declared<Task>>,
    pub templates: BTreeMap<String, Declared<ScriptTemplate>>,
    pub ssh_grant: Option<Declared<SshGrant>>,
}

fn merge_entries<T>(
    into: &mut BTreeMap<String, Declared<T>>,
    entries: BTreeMap<String, T>,
    origin: &Origin,
    group: &str,
    is_enabled: impl Fn(&T) -> bool,
) -> anyhow::Result<()> {
    for (id, value) in entries {
        if id.is_empty()
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        {
            bail!(
                "{}: {group}.{id}: IDs must use letters, numbers, _ or -",
                origin.profile
            );
        }
        if is_enabled(&value) {
            into.insert(
                id.clone(),
                Declared {
                    value,
                    origin: origin.at(&format!("{group}.{id}")),
                },
            );
        } else {
            into.remove(&id);
        }
    }
    Ok(())
}

impl Resolved {
    fn merge(&mut self, profile: Profile, origin: Origin) -> anyhow::Result<()> {
        macro_rules! scalar {
            ($field:ident) => {
                if profile.shell.$field.is_some() {
                    self.shell.$field = profile.shell.$field;
                    self.origins.insert(
                        concat!("shell.", stringify!($field)).into(),
                        origin.at(concat!("shell.", stringify!($field))),
                    );
                }
            };
        }
        scalar!(program);
        scalar!(path);
        scalar!(change_login_shell);
        for (key, value) in profile.env {
            validate_env_key(&key).with_context(|| origin.at(&format!("env.{key}")).context())?;
            self.origins
                .insert(format!("env.{key}"), origin.at(&format!("env.{key}")));
            self.env.insert(key, value);
        }
        for (phase, edits) in [
            (Phase::Pre, profile.packages.pre),
            (Phase::Post, profile.packages.post),
        ] {
            let packages = self.packages.entry(phase).or_default();
            let removed = self.removed_packages.entry(phase).or_default();
            for package in edits.add {
                let name = package_name(&package).with_context(|| {
                    origin
                        .at(&format!("packages.{}.add", phase.name()))
                        .context()
                })?;
                removed.retain(|p| p != name);
                if !packages.iter().any(|p| p.value == package) {
                    packages.push(Declared {
                        value: package,
                        origin: origin.at(&format!("packages.{}.add", phase.name())),
                    });
                }
            }
            for package in edits.remove {
                let name = package_name(&package)
                    .with_context(|| {
                        origin
                            .at(&format!("packages.{}.remove", phase.name()))
                            .context()
                    })?
                    .to_owned();
                packages.retain(|p| package_name(&p.value).ok() != Some(name.as_str()));
                removed.push(name);
            }
        }
        merge_entries(&mut self.files, profile.files, &origin, "files", |v| {
            v.enabled
        })?;
        merge_entries(&mut self.tasks, profile.tasks, &origin, "tasks", |v| {
            v.enabled
        })?;
        merge_entries(
            &mut self.templates,
            profile.templates,
            &origin,
            "templates",
            |v| v.enabled,
        )?;
        if let Some(grant) = profile.ssh_grant {
            self.ssh_grant = Some(Declared { value: grant, origin: origin.at("ssh_grant") });
        }
        self.profiles.push(origin.profile);
        Ok(())
    }
}

pub fn validate_env_key(key: &str) -> anyhow::Result<()> {
    if key.is_empty()
        || key.as_bytes()[0].is_ascii_digit()
        || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        bail!("invalid environment variable name {key:?}");
    }
    Ok(())
}

pub fn package_name(package: &str) -> anyhow::Result<&str> {
    let (name, version) = package
        .split_once('=')
        .map_or((package, None), |(n, v)| (n, Some(v)));
    let (base, arch) = name
        .split_once(':')
        .map_or((name, None), |(base, arch)| (base, Some(arch)));
    if base.len() < 2
        || !base.as_bytes()[0].is_ascii_alphanumeric()
        || !base
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"+.-".contains(&c))
        || arch.is_some_and(|a| {
            a.is_empty()
                || !a.as_bytes()[0].is_ascii_alphanumeric()
                || !a
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
        || version.is_some_and(|v| {
            v.is_empty()
                || !v
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".+~:-".contains(&c))
        })
    {
        bail!("invalid package {package:?}; expected a Debian name[:arch][=version]");
    }
    Ok(name)
}

/// Shared bases are applied for every occurrence, in DFS order. Only the
/// active stack detects cycles: a diamond is legal and last applied wins.
pub fn resolve(selection: &str) -> anyhow::Result<Resolved> {
    fn visit(
        selection: &str,
        directory: &Path,
        stack: &mut Vec<String>,
        out: &mut Resolved,
    ) -> anyhow::Result<()> {
        let (identity, text, directory) = match selection {
            "builtin:core" => (
                selection.to_owned(),
                include_str!("../../../environments/core.toml").to_owned(),
                None,
            ),
            "builtin:development" => (
                selection.to_owned(),
                include_str!("../../../environments/development.toml").to_owned(),
                None,
            ),
            name if name.starts_with("builtin:") => bail!("unknown bundled profile {name:?}"),
            path => {
                let path = local_path(path, directory)?
                    .canonicalize()
                    .with_context(|| {
                        format!(
                            "opening profile {selection:?} relative to {}",
                            directory.display()
                        )
                    })?;
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                (
                    path.display().to_string(),
                    text,
                    Some(path.parent().unwrap().to_owned()),
                )
            }
        };
        if stack.contains(&identity) {
            stack.push(identity);
            bail!("profile inheritance cycle: {}", stack.join(" -> "));
        }
        let profile: Profile =
            toml::from_str(&text).with_context(|| format!("parsing {identity}"))?;
        if profile.schema_version != 1 {
            bail!("{identity}: schema_version must be 1");
        }
        stack.push(identity.clone());
        for base in &profile.extends {
            if directory.is_none() && !base.starts_with("builtin:") {
                bail!("{identity}: bundled bases must be builtin: names");
            }
            visit(
                base,
                directory.as_deref().unwrap_or(Path::new(".")),
                stack,
                out,
            )?;
        }
        stack.pop();
        out.merge(
            profile,
            Origin {
                profile: identity,
                key: String::new(),
                directory,
            },
        )
    }
    let mut out = Resolved::default();
    visit(
        selection,
        &std::env::current_dir()?,
        &mut Vec::new(),
        &mut out,
    )?;
    Ok(out)
}
