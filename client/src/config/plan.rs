//! Offline compilation only. Nodes describe the future launch executor's
//! inputs; this module neither schedules work nor reads source tree contents.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use anyhow::{bail, Context};
use beams_protocol::{task_catalog, ManifestParam};
use serde::Serialize;

use super::profile::{
    self, Declared, FileMode, Origin, Overwrite, Phase, Rerun, Resolved, Source, Task,
};

#[derive(Debug, Serialize)]
pub struct LaunchPlan {
    pub schema_version: u32,
    pub shell: profile::Shell,
    pub env: BTreeMap<String, String>,
    pub artifacts: Vec<Artifact>,
    /// Stable topological order, not mandatory execution waves.
    pub nodes: Vec<Node>,
}

#[derive(Debug, Serialize)]
pub struct Artifact {
    pub id: String,
    pub phase: Phase,
    pub directory: bool,
    pub exclude: Vec<String>,
    pub used_by: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub phase: Phase,
    pub after: Vec<String>,
    pub resources: Vec<String>,
    pub action: Action,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Action {
    Condition,
    Runtime,
    Packages {
        packages: Vec<String>,
    },
    File {
        artifact: String,
        target: String,
        mode: FileMode,
        overwrite: Overwrite,
    },
    Script {
        artifact: String,
        entrypoint: String,
        interpreter: Vec<String>,
        args: Vec<String>,
        working_dir: Option<String>,
        env: BTreeMap<String, String>,
        check: Option<Vec<String>>,
        rerun: Rerun,
    },
    Builtin {
        template: String,
        params: BTreeMap<String, ManifestParam>,
    },
}

/// Workstation-only explanation/snapshot inputs. Authoritative source paths
/// never belong to the remote plan.
#[derive(Debug, Serialize)]
pub struct Compilation {
    pub profiles: Vec<String>,
    pub origins: BTreeMap<String, Origin>,
    pub sources: BTreeMap<String, Source>,
    pub notes: Vec<String>,
    pub plan: LaunchPlan,
}

struct Compiler {
    nodes: Vec<Node>,
    artifacts: Vec<Artifact>,
    sources: BTreeMap<String, Source>,
    origins: BTreeMap<String, Origin>,
    notes: Vec<String>,
    packages: BTreeMap<Phase, Vec<Declared<String>>>,
    removed_packages: BTreeMap<Phase, Vec<String>>,
}

impl Compiler {
    fn node(
        &mut self,
        id: &str,
        label: String,
        phase: Phase,
        after: Vec<String>,
        resources: Vec<String>,
        action: Action,
        origin: &Origin,
    ) {
        self.origins.insert(id.to_owned(), origin.clone());
        self.nodes.push(Node {
            id: id.to_owned(),
            label,
            phase,
            after,
            resources,
            action,
        });
    }

    fn artifact(
        &mut self,
        source: Source,
        directory: bool,
        exclude: &[String],
        phase: Phase,
        user: &str,
        origin: &Origin,
    ) -> anyhow::Result<String> {
        for path in exclude {
            if path.is_empty()
                || path.contains(['*', '?', '[', ']'])
                || Path::new(path).is_absolute()
                || Path::new(path)
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_)))
            {
                bail!(
                    "{}: exclude entries must be relative path prefixes",
                    origin.context()
                );
            }
        }
        let mut selection = exclude.to_vec();
        selection.sort();
        selection.dedup();
        if let Some(artifact) = self
            .artifacts
            .iter_mut()
            .find(|a| self.sources.get(&a.id) == Some(&source) && a.exclude == selection)
        {
            artifact.used_by.push(user.to_owned());
            if phase < artifact.phase {
                artifact.phase = phase;
                self.notes.push(format!(
                    "{} promoted to pre: {user} shares its source root",
                    artifact.id
                ));
            } else if artifact.phase == Phase::Pre
                && phase == Phase::Post
                && !self
                    .notes
                    .iter()
                    .any(|n| n.starts_with(&format!("{} ", artifact.id)))
            {
                self.notes.push(format!(
                    "{} staged in pre: shared root also used by {user}",
                    artifact.id
                ));
            }
            return Ok(artifact.id.clone());
        }
        let id = format!("artifact-{}", self.artifacts.len() + 1);
        self.artifacts.push(Artifact {
            id: id.clone(),
            phase,
            directory,
            exclude: selection,
            used_by: vec![user.to_owned()],
        });
        self.sources.insert(id.clone(), source);
        self.origins.insert(id.clone(), origin.clone());
        Ok(id)
    }

    fn require_package(
        &mut self,
        package: &str,
        phase: Phase,
        origin: &Origin,
    ) -> anyhow::Result<()> {
        let name = profile::package_name(package).with_context(|| origin.context())?;
        let supplied_pre = phase == Phase::Post
            && self.packages.get(&Phase::Pre).is_some_and(|packages| {
                packages
                    .iter()
                    .any(|p| profile::package_name(&p.value).ok() == Some(name))
            });
        if !supplied_pre
            && self
                .removed_packages
                .get(&phase)
                .is_some_and(|p| p.iter().any(|p| p == name))
        {
            bail!(
                "{}: required package {package} was removed from {}; disable its consumer instead",
                origin.context(),
                phase.name()
            );
        }
        self.packages.entry(phase).or_default().push(Declared {
            value: package.to_owned(),
            origin: origin.clone(),
        });
        Ok(())
    }
}

fn system_origin(key: &str) -> Origin {
    Origin {
        profile: "Beams compiler".into(),
        key: key.into(),
        directory: None,
    }
}

/// Normalize remote targets without expanding the workstation's HOME.
pub fn remote_path(input: &str) -> anyhow::Result<String> {
    let input = input
        .strip_prefix("~/")
        .map(|s| format!("${{beam.home}}/{s}"))
        .unwrap_or_else(|| {
            if input == "~" {
                "${beam.home}".into()
            } else {
                input.into()
            }
        });
    let (prefix, tail) = if input == "${beam.home}" {
        ("${beam.home}", "")
    } else if let Some(tail) = input.strip_prefix("${beam.home}/") {
        ("${beam.home}", tail)
    } else if let Some(tail) = input.strip_prefix('/') {
        ("", tail)
    } else {
        bail!("remote path {input:?} must be absolute or start with ~/ or ${{beam.home}}/");
    };
    if input.contains('\0') || input.contains('\n') || tail.contains("${") {
        bail!("invalid remote path {input:?}");
    }
    let mut parts = Vec::new();
    for part in tail.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    bail!("remote path escapes its root: {input}");
                }
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        Ok(if prefix.is_empty() {
            "/".into()
        } else {
            prefix.into()
        })
    } else {
        Ok(format!("{prefix}/{}", parts.join("/")))
    }
}

fn validate_params(template: &str, params: &BTreeMap<String, ManifestParam>) -> anyhow::Result<()> {
    let schema = task_catalog::templates()
        .into_iter()
        .find(|t| t.template == template)
        .with_context(|| format!("unknown built-in template {template:?}"))?;
    for key in params.keys() {
        if !schema.params.iter().any(|p| p.name == key) {
            bail!("{template}: unknown parameter {key}");
        }
    }
    for param in schema.params {
        match params.get(param.name) {
            None if param.required => bail!("{template}: missing parameter {}", param.name),
            Some(value) => validate_kind(param.kind, value)
                .with_context(|| format!("{template}: parameter {}", param.name))?,
            None => {}
        }
    }
    if template == "checkout-pr-worktree" {
        if let Some(ManifestParam::Int(pr)) = params.get("pr") {
            if *pr < 1 || *pr > u32::MAX as i64 {
                bail!("checkout-pr-worktree: pr must be 1..={}", u32::MAX);
            }
        }
    }
    Ok(())
}

fn validate_kind(kind: &str, value: &ManifestParam) -> anyhow::Result<()> {
    match (kind, value) {
        ("string", ManifestParam::Str(_))
        | ("boolean", ManifestParam::Bool(_))
        | ("number", ManifestParam::Int(_)) => Ok(()),
        ("path", ManifestParam::Str(path)) => {
            remote_path(path)?;
            Ok(())
        }
        ("string" | "path" | "boolean" | "number", _) => bail!("expected {kind}"),
        _ => bail!("unknown parameter kind {kind:?}"),
    }
}

fn param_string(value: &ManifestParam) -> String {
    match value {
        ManifestParam::Str(v) => v.clone(),
        ManifestParam::Int(v) => v.to_string(),
        ManifestParam::Bool(v) => v.to_string(),
    }
}

fn substitute(input: &str, params: &BTreeMap<String, ManifestParam>) -> anyhow::Result<String> {
    let mut remaining = input;
    let mut out = String::new();
    while let Some(start) = remaining.find("${param.") {
        out.push_str(&remaining[..start]);
        let tail = &remaining[start + 8..];
        let end = tail
            .find('}')
            .context("unterminated ${param.NAME} substitution")?;
        let name = &tail[..end];
        out.push_str(&param_string(
            params
                .get(name)
                .with_context(|| format!("unknown substitution parameter {name}"))?,
        ));
        remaining = &tail[end + 1..];
    }
    out.push_str(remaining);
    Ok(out)
}

fn materialize_custom(
    task: &Declared<Task>,
    resolved: &Resolved,
) -> anyhow::Result<Declared<Task>> {
    let Some(name) = task
        .value
        .template
        .as_deref()
        .and_then(|t| t.strip_prefix("user:"))
    else {
        return Ok(task.clone());
    };
    let declared = resolved.templates.get(name).with_context(|| {
        format!(
            "{}: unknown custom template {name:?}",
            task.origin.context()
        )
    })?;
    let template = &declared.value;
    let mut params = task.value.params.clone();
    for name in params.keys() {
        if !template.params.contains_key(name) {
            bail!("unknown custom parameter {name:?}");
        }
    }
    for (name, schema) in &template.params {
        if !params.contains_key(name) {
            params.insert(
                name.clone(),
                schema
                    .default
                    .clone()
                    .with_context(|| format!("missing custom parameter {name:?}"))?,
            );
        }
        validate_kind(&schema.kind, &params[name]).with_context(|| format!("parameter {name}"))?;
    }
    let subst_vec = |values: &[String]| -> anyhow::Result<Vec<String>> {
        values.iter().map(|s| substitute(s, &params)).collect()
    };
    let mut value = task.value.clone();
    // Source paths always belong to the definition, not to its instance or params.
    value.template = None;
    value.params.clear();
    value.script = template.script.clone();
    value.script_root = template.script_root.clone();
    value.exclude = template.exclude.clone();
    value.label = value.label.or_else(|| template.label.clone());
    value.interpreter = Some(subst_vec(
        template.interpreter.as_deref().unwrap_or(&["bash".into()]),
    )?);
    value.args = subst_vec(&template.args)?;
    value.working_dir = template
        .working_dir
        .as_deref()
        .map(|s| substitute(s, &params))
        .transpose()?;
    value.env = template
        .env
        .iter()
        .map(|(k, v)| Ok((k.clone(), substitute(v, &params)?)))
        .collect::<anyhow::Result<_>>()?;
    value.check = template.check.as_ref().map(|v| subst_vec(v)).transpose()?;
    value.packages.extend(template.packages.clone());
    value.resources.extend(template.resources.clone());
    Ok(Declared {
        value,
        origin: declared.origin.clone(),
    })
}

fn param<'a>(task: &'a Task, key: &str) -> &'a str {
    match &task.params[key] {
        ManifestParam::Str(v) => v,
        _ => unreachable!("validated string param"),
    }
}

fn builtin_edges(task: &Task) -> anyhow::Result<(Vec<String>, Vec<String>, Vec<String>)> {
    let mut provides = Vec::new();
    let mut requires = Vec::new();
    let mut resources = Vec::new();
    match task.template.as_deref().unwrap() {
        "install-toolchain-go" => {
            provides.push("go-toolchain-installed".into());
            resources.push("path:${beam.home}/.local/toolchains/go".into());
        }
        "install-toolchain-rust" => {
            provides.push("rust-toolchain-installed".into());
            resources.extend([
                "path:${beam.home}/.cargo".into(),
                "path:${beam.home}/.rustup".into(),
            ]);
        }
        "install-code-server" => {
            provides.push("code-server-running".into());
            requires.extend([
                "go-toolchain-installed".into(),
                "rust-toolchain-installed".into(),
            ]);
            resources.push("path:${beam.home}/.local/toolchains/code-server".into());
        }
        "wait-ssh-agent" => provides.push("ssh-agent-ready".into()),
        "clone-git-repo" => {
            let dest = remote_path(param(task, "dest"))?;
            provides.push(format!("repo-cloned:{dest}"));
            resources.push(format!("path:{dest}"));
            let repo = param(task, "repo");
            if repo.starts_with("git@") || repo.starts_with("ssh://") {
                requires.push("ssh-agent-ready".into());
            }
        }
        "checkout-pr-worktree" => {
            let dest = remote_path(param(task, "dest"))?;
            provides.push(format!("pr-worktree:{dest}"));
            requires.push(format!("repo-cloned:{}", remote_path(param(task, "base"))?));
            resources.push(format!("path:{dest}"));
        }
        template => bail!("built-in template {template:?} has no launch compiler rules yet"),
    }
    Ok((provides, requires, resources))
}

fn implicit_task(template: &str, phase: Phase) -> Task {
    // Using the same deserializer keeps all schema defaults in one place.
    toml::from_str(&format!(
        "template = {template:?}\nphase = {:?}\n",
        phase.name()
    ))
    .unwrap()
}

fn prerequisite_tasks(tasks: &mut BTreeMap<String, Declared<Task>>) -> anyhow::Result<Vec<String>> {
    let mut notes = Vec::new();
    // Only these unparameterized setup requirements have unambiguous providers.
    let builtins = [
        ("go-toolchain-installed", "install-toolchain-go"),
        ("rust-toolchain-installed", "install-toolchain-rust"),
        ("ssh-agent-ready", "wait-ssh-agent"),
    ];
    loop {
        let mut needed: BTreeMap<&str, Phase> = BTreeMap::new();
        for declared in tasks.values() {
            if declared.value.template.is_some() {
                let (_, requirements, _) = builtin_edges(&declared.value)?;
                for (cap, template) in builtins {
                    if requirements.iter().any(|r| r == cap) {
                        needed
                            .entry(template)
                            .and_modify(|phase| *phase = (*phase).min(declared.value.phase))
                            .or_insert(declared.value.phase);
                    }
                }
            }
        }
        let mut changed = false;
        for (template, phase) in needed {
            if tasks
                .values()
                .any(|t| t.value.template.as_deref() == Some(template))
            {
                // Explicit post providers remain explicit; inversion is rejected below.
                let id = format!("auto-{template}");
                if let Some(task) = tasks.get_mut(&id) {
                    if task.origin.profile == "Beams compiler" && phase < task.value.phase {
                        task.value.phase = phase;
                        notes.push(format!(
                            "tasks:{id} promoted to pre for a required capability"
                        ));
                        changed = true;
                    }
                }
                continue;
            }
            let id = format!("auto-{template}");
            if tasks.contains_key(&id) {
                bail!("reserved automatic task ID {id} conflicts with a configured task");
            }
            tasks.insert(
                id.clone(),
                Declared {
                    value: implicit_task(template, phase),
                    origin: system_origin(&format!("tasks.{id}")),
                },
            );
            notes.push(format!(
                "tasks:{id} added in {} for a required capability",
                phase.name()
            ));
            changed = true;
        }
        if !changed {
            break;
        }
    }
    Ok(notes)
}

pub fn compile(selection: &str) -> anyhow::Result<Compilation> {
    let mut resolved = profile::resolve(selection)?;
    let mut tasks = BTreeMap::new();
    // Validate template definitions even if they have no instances.
    for declared in resolved.templates.values() {
        let template = &declared.value;
        let source = template
            .script
            .as_deref()
            .context("custom template needs script")
            .with_context(|| declared.origin.context())?;
        validate_script_source(&declared.origin, source, template.script_root.as_deref())?;
        for (name, schema) in &template.params {
            if !["string", "path", "boolean", "number"].contains(&schema.kind.as_str()) {
                bail!(
                    "{}: parameter {name} has unknown kind {:?}",
                    declared.origin.context(),
                    schema.kind
                );
            }
            if let Some(default) = &schema.default {
                validate_kind(&schema.kind, default)?;
            }
        }
        for path in &template.exclude {
            if path.is_empty()
                || path.contains(['*', '?', '[', ']'])
                || Path::new(path).is_absolute()
                || Path::new(path)
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_)))
            {
                bail!(
                    "{}: exclude entries must be relative path prefixes",
                    declared.origin.context()
                );
            }
        }
        for key in template.env.keys() {
            profile::validate_env_key(key).with_context(|| declared.origin.context())?;
        }
    }
    for (id, task) in &resolved.tasks {
        if task.value.script.is_some() == task.value.template.is_some() {
            bail!(
                "{}: specify exactly one of script or template",
                task.origin.context()
            );
        }
        if task.value.template.is_some()
            && (task.value.script_root.is_some()
                || task.value.interpreter.is_some()
                || !task.value.args.is_empty()
                || task.value.working_dir.is_some()
                || !task.value.env.is_empty()
                || task.value.check.is_some()
                || !task.value.exclude.is_empty())
        {
            bail!(
                "{}: script execution fields belong to the template definition",
                task.origin.context()
            );
        }
        let mut task =
            materialize_custom(task, &resolved).with_context(|| task.origin.context())?;
        if let Some(template) = &task.value.template {
            validate_params(template, &task.value.params).with_context(|| task.origin.context())?;
            // Canonical remote params make providers independent of ~/ vs ${beam.home}.
            for key in ["dest", "base"] {
                if let Some(ManifestParam::Str(path)) = task.value.params.get_mut(key) {
                    *path = remote_path(path)?;
                }
            }
        } else if !task.value.params.is_empty() {
            bail!("{}: params require a template", task.origin.context());
        }
        tasks.insert(id.clone(), task);
    }
    let mut compiler = Compiler {
        nodes: Vec::new(),
        artifacts: Vec::new(),
        sources: BTreeMap::new(),
        origins: resolved.origins.clone(),
        notes: prerequisite_tasks(&mut tasks)?,
        packages: std::mem::take(&mut resolved.packages),
        removed_packages: std::mem::take(&mut resolved.removed_packages),
    };
    let system = system_origin("readiness");
    compiler.node(
        "artifact-ready:pre",
        "Verified pre archive".into(),
        Phase::Pre,
        vec![],
        vec![],
        Action::Condition,
        &system,
    );
    compiler.node(
        "runtime-ready",
        "Core runtime ready".into(),
        Phase::Pre,
        vec!["artifact-ready:pre".into()],
        vec![],
        Action::Runtime,
        &system,
    );
    compiler.node(
        "interface-ready",
        "Shell and first TUI frame ready".into(),
        Phase::Post,
        vec![],
        vec![],
        Action::Condition,
        &system,
    );
    compiler.node(
        "artifact-ready:post",
        "Verified post archive".into(),
        Phase::Post,
        vec!["interface-ready".into()],
        vec![],
        Action::Condition,
        &system,
    );

    let shell_program = resolved.shell.program.get_or_insert("/bin/bash".into());
    if !shell_program.starts_with('/') || shell_program.contains('\0') {
        bail!("shell.program must be an absolute remote executable path");
    }
    if shell_program == "/usr/bin/fish" {
        compiler.require_package(
            "fish",
            Phase::Pre,
            resolved.origins.get("shell.program").unwrap_or(&system),
        )?;
    }
    for path in resolved.shell.path.get_or_insert_with(Vec::new) {
        *path = remote_path(path).context("shell.path")?;
    }
    resolved.shell.change_login_shell.get_or_insert(false);

    for (id, declared) in &resolved.files {
        let file = &declared.value;
        let origin = &declared.origin;
        let result = (|| {
            let input = file.source.as_deref().context("missing source")?;
            let source = origin.source(input)?;
            let directory = match &source {
                Source::Local(p) => {
                    if !p.is_file() && !p.is_dir() {
                        bail!("source must be a regular file or directory");
                    }
                    p.is_dir()
                }
                Source::Embedded(_) => false,
            };
            let target = remote_path(file.target.as_deref().context("missing target")?)?;
            if target == "/" || target == "${beam.home}" {
                bail!("file target must not replace a root directory");
            }
            if !directory && !file.exclude.is_empty() {
                bail!("exclude requires a directory source");
            }
            let node_id = format!("files:{id}");
            let artifact = compiler.artifact(
                source,
                directory,
                &file.exclude,
                file.phase,
                &node_id,
                origin,
            )?;
            let mut after = file.after.clone();
            after.push("runtime-ready".into());
            compiler.node(
                &node_id,
                format!("Install {id}"),
                file.phase,
                after,
                vec![format!("path:{target}")],
                Action::File {
                    artifact,
                    target,
                    mode: file.mode,
                    overwrite: file.overwrite,
                },
                origin,
            );
            Ok(())
        })();
        result.with_context(|| origin.context())?;
    }

    let mut providers: BTreeMap<String, String> = BTreeMap::new();
    let mut required = Vec::new();
    let mut package_needs = Vec::new();
    for (id, declared) in tasks {
        let task = declared.value;
        let origin = declared.origin;
        let node_id = format!("tasks:{id}");
        if let Some(instance) = resolved.tasks.get(&id) {
            compiler
                .origins
                .insert(format!("instance:{node_id}"), instance.origin.clone());
        }
        let result = (|| {
            let mut after = task.after.clone();
            let mut resources = task.resources.clone();
            for resource in &mut resources {
                if let Some(path) = resource.strip_prefix("path:") {
                    *resource = format!("path:{}", remote_path(path)?);
                }
                if resource.is_empty() {
                    bail!("empty resource");
                }
            }
            let mut packages = task.packages.clone();
            let action = if let Some(template) = &task.template {
                let (provides, requires, builtin_resources) = builtin_edges(&task)?;
                resources.extend(builtin_resources);
                for capability in provides {
                    if let Some(other) = providers.insert(capability.clone(), node_id.clone()) {
                        bail!("duplicate capability {capability}: {other} and {node_id}");
                    }
                }
                required.push((node_id.clone(), requires));
                if template == "install-toolchain-rust" {
                    packages.push("gcc".into());
                }
                Action::Builtin {
                    template: template.clone(),
                    params: task.params.clone(),
                }
            } else {
                let (source, entrypoint) = validate_script_source(
                    &origin,
                    task.script.as_deref().unwrap(),
                    task.script_root.as_deref(),
                )?;
                for exclusion in &task.exclude {
                    if entrypoint == *exclusion || entrypoint.starts_with(&format!("{exclusion}/"))
                    {
                        bail!("script entrypoint is excluded by {exclusion:?}");
                    }
                }
                let directory = task.script_root.is_some();
                let artifact = compiler.artifact(
                    source,
                    directory,
                    &task.exclude,
                    task.phase,
                    &node_id,
                    &origin,
                )?;
                let interpreter = task
                    .interpreter
                    .clone()
                    .unwrap_or_else(|| vec!["bash".into()]);
                if interpreter.first().is_none_or(|s| s.is_empty()) {
                    bail!("interpreter must have a nonempty executable");
                }
                if task
                    .check
                    .as_ref()
                    .is_some_and(|v| v.first().is_none_or(|s| s.is_empty()))
                {
                    bail!("check must have a nonempty executable");
                }
                for key in task.env.keys() {
                    profile::validate_env_key(key)?;
                }
                if let Some(cwd) = &task.working_dir {
                    if cwd.starts_with('/')
                        || cwd.starts_with('~')
                        || cwd.starts_with("${beam.home}")
                    {
                        remote_path(cwd)?;
                    } else if cwd.is_empty() || cwd.contains('\0') {
                        bail!("invalid working_dir");
                    }
                }
                Action::Script {
                    artifact,
                    entrypoint,
                    interpreter,
                    args: task.args.clone(),
                    working_dir: task.working_dir.clone(),
                    env: task.env.clone(),
                    check: task.check.clone(),
                    rerun: task.rerun,
                }
            };
            for package in packages {
                compiler.require_package(&package, task.phase, &origin)?;
                package_needs.push((node_id.clone(), profile::package_name(&package)?.to_owned()));
            }
            after.push("runtime-ready".into());
            compiler.node(
                &node_id,
                task.label.clone().unwrap_or(id.clone()),
                task.phase,
                after,
                resources,
                action,
                &origin,
            );
            Ok(())
        })();
        result.with_context(|| origin.context())?;
    }
    for (node, requirements) in required {
        for capability in requirements {
            let provider = providers
                .get(&capability)
                .with_context(|| format!("{node}: missing provider for {capability}"))?;
            compiler
                .nodes
                .iter_mut()
                .find(|n| n.id == node)
                .unwrap()
                .after
                .push(provider.clone());
        }
    }
    let (batches, package_phases) = package_batches(&compiler.packages)?;
    for (declared_phase, packages) in &compiler.packages {
        for package in packages {
            let name = profile::package_name(&package.value)?;
            let selected_phase = package_phases[name];
            compiler
                .origins
                .entry(format!(
                    "package:{}:{}",
                    selected_phase.name(),
                    package.value
                ))
                .or_insert_with(|| package.origin.clone());
            if *declared_phase > selected_phase {
                let note = format!(
                    "package {name} assigned to pre; also required in post ({})",
                    package.origin.context()
                );
                if !compiler.notes.contains(&note) {
                    compiler.notes.push(note);
                }
            }
        }
    }
    for (node, package) in package_needs {
        let phase = package_phases[&package];
        compiler
            .nodes
            .iter_mut()
            .find(|n| n.id == node)
            .unwrap()
            .after
            .push(format!("packages:{}", phase.name()));
    }
    for phase in [Phase::Pre, Phase::Post] {
        let packages = batches.get(&phase).cloned().unwrap_or_default();
        compiler.node(
            &format!("packages:{}", phase.name()),
            format!("{} packages", phase.name()),
            phase,
            vec!["runtime-ready".into()],
            vec!["apt".into()],
            Action::Packages { packages },
            &system_origin("package batching"),
        );
    }

    // Artifact phase is finalized after all consumers have been seen.
    for node in &mut compiler.nodes {
        if let Action::File { artifact, .. } | Action::Script { artifact, .. } = &node.action {
            let phase = compiler
                .artifacts
                .iter()
                .find(|a| &a.id == artifact)
                .unwrap()
                .phase;
            node.after.push(format!("artifact-ready:{}", phase.name()));
        }
        if node.phase == Phase::Post && node.id != "interface-ready" {
            node.after.push("interface-ready".into());
        }
    }
    let pre = compiler
        .nodes
        .iter()
        .filter(|n| n.phase == Phase::Pre)
        .map(|n| n.id.clone())
        .collect();
    compiler
        .nodes
        .iter_mut()
        .find(|n| n.id == "interface-ready")
        .unwrap()
        .after = pre;
    let ordered = order_nodes(compiler.nodes, &compiler.origins)?;
    validate_destinations(&ordered)?;
    Ok(Compilation {
        profiles: resolved.profiles,
        origins: compiler.origins,
        sources: compiler.sources,
        notes: compiler.notes,
        plan: LaunchPlan {
            schema_version: 1,
            shell: resolved.shell,
            env: resolved.env,
            artifacts: compiler.artifacts,
            nodes: ordered,
        },
    })
}

fn validate_script_source(
    origin: &Origin,
    script: &str,
    root: Option<&str>,
) -> anyhow::Result<(Source, String)> {
    let entry = origin.source(script)?;
    let Source::Local(entry) = entry else {
        bail!("bundled script sources are not defined yet");
    };
    if !entry.is_file() {
        bail!("{}: script must be a file", origin.context());
    }
    if let Some(root) = root {
        let source = origin.source(root)?;
        let Source::Local(path) = &source else {
            bail!("script_root must be a local directory");
        };
        if !path.is_dir() {
            bail!("script_root must be a directory");
        }
        let relative = entry
            .strip_prefix(path)
            .context("script entrypoint must be inside script_root")?
            .to_str()
            .context("script entrypoint must be UTF-8")?
            .to_owned();
        Ok((source, relative))
    } else {
        let filename = entry
            .file_name()
            .unwrap()
            .to_str()
            .context("script filename must be UTF-8")?
            .to_owned();
        Ok((Source::Local(entry), filename))
    }
}

fn package_batches(
    packages: &BTreeMap<Phase, Vec<Declared<String>>>,
) -> anyhow::Result<(BTreeMap<Phase, Vec<String>>, BTreeMap<String, Phase>)> {
    let mut names = Vec::new();
    let mut selected: BTreeMap<String, (String, Phase, String)> = BTreeMap::new();
    for (phase, packages) in packages {
        for declared in packages {
            let package = &declared.value;
            let name = profile::package_name(package)?.to_owned();
            if let Some((current, current_phase, origin)) = selected.get_mut(&name) {
                if current.contains('=') && package.contains('=') && current != package {
                    bail!(
                        "{}: conflicting package versions {package} and {current} from {origin}",
                        declared.origin.context()
                    );
                }
                if package.contains('=') {
                    *current = package.clone();
                }
                *current_phase = (*current_phase).min(*phase);
            } else {
                names.push(name.clone());
                selected.insert(name, (package.clone(), *phase, declared.origin.context()));
            }
        }
    }
    let mut batches: BTreeMap<Phase, Vec<String>> = BTreeMap::new();
    let mut phases = BTreeMap::new();
    for name in names {
        let (package, phase, _) = selected.remove(&name).unwrap();
        batches.entry(phase).or_default().push(package);
        phases.insert(name, phase);
    }
    Ok((batches, phases))
}

fn order_nodes(
    mut nodes: Vec<Node>,
    origins: &BTreeMap<String, Origin>,
) -> anyhow::Result<Vec<Node>> {
    let phases: BTreeMap<_, _> = nodes.iter().map(|n| (n.id.clone(), n.phase)).collect();
    if phases.len() != nodes.len() {
        bail!("duplicate graph node ID");
    }
    for node in &mut nodes {
        node.after.sort();
        node.after.dedup();
        node.resources.sort();
        node.resources.dedup();
        for dependency in &node.after {
            let phase = phases.get(dependency).with_context(|| {
                format!(
                    "{}: unknown dependency {dependency}",
                    origins[&node.id].context()
                )
            })?;
            if node.phase == Phase::Pre && *phase == Phase::Post {
                bail!(
                    "{}: pre task {} depends on post node {dependency}",
                    origins[&node.id].context(),
                    node.id
                );
            }
        }
    }
    let mut ordered = Vec::new();
    let mut done = BTreeSet::new();
    while !nodes.is_empty() {
        let Some(index) = nodes
            .iter()
            .position(|n| n.after.iter().all(|dep| done.contains(dep)))
        else {
            bail!(
                "dependency cycle among: {}",
                nodes
                    .iter()
                    .map(|n| format!("{} ({})", n.id, origins[&n.id].context()))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        };
        let node = nodes.remove(index);
        done.insert(node.id.clone());
        ordered.push(node);
    }
    Ok(ordered)
}

fn depends_on(nodes: &[Node], from: &str, target: &str) -> bool {
    let mut pending = vec![from];
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        for dependency in &nodes.iter().find(|n| n.id == id).unwrap().after {
            if dependency == target {
                return true;
            }
            pending.push(dependency);
        }
    }
    false
}

fn validate_destinations(nodes: &[Node]) -> anyhow::Result<()> {
    let destinations: Vec<_> = nodes
        .iter()
        .filter_map(|n| match &n.action {
            Action::File { target, .. } => Some((n, target.clone())),
            Action::Builtin { template, params }
                if template == "clone-git-repo" || template == "checkout-pr-worktree" =>
            {
                Some((n, param_string(&params["dest"])))
            }
            _ => None,
        })
        .collect();
    for (index, (left, a)) in destinations.iter().enumerate() {
        for (right, b) in &destinations[index + 1..] {
            if a == b {
                bail!("duplicate destination {a}: {} and {}", left.id, right.id);
            }
            if (a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/")))
                && !depends_on(nodes, &left.id, &right.id)
                && !depends_on(nodes, &right.id, &left.id)
            {
                bail!(
                    "overlapping destinations {a} ({}) and {b} ({}); add an explicit after edge",
                    left.id,
                    right.id
                );
            }
        }
    }
    Ok(())
}
