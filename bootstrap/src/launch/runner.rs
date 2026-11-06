//! Launch-scoped adapters for the existing executor. Headless release is an
//! explicit policy, never a substitute for an automatic Zellij/TUI signal.
use crate::{executor, graph::Capability, tasks::Task};
use anyhow::{bail, Context as _};
use beams_protocol::{launch::*, ManifestParam};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// A phase's verified staging: (canonical staging dir, artifact) pairs.
pub type Verified = Arc<Vec<(PathBuf, PackedArtifact)>>;

/// How a launch's external conditions (`interface-ready`,
/// `artifact-ready:<phase>`) are released: checked once by the foreground
/// CLI (`ForegroundConditions`), waited for by the daemon
/// (`launch::daemon`).
pub trait Conditions: Send + Sync {
    /// Runs before any effect of `phase`; may refuse the run.
    fn prepare(&self, declaration: &Declaration, phase: Phase) -> anyhow::Result<()>;
    /// Returns once `condition` is released, or fails if it can't be.
    fn wait(&self, condition: &str, progress: &dyn Fn(&str)) -> anyhow::Result<()>;
    /// The verified artifacts of `phase`, once released.
    fn verified(&self, phase: Phase) -> Option<Verified>;
}

/// The foreground `run-phase` policy: staging that is present is verified
/// up front (before effects); a missing bundle fails its condition, and
/// `interface-ready` releases only with explicit `--headless`.
pub struct ForegroundConditions {
    staging: PathBuf,
    headless: bool,
    verified: Mutex<BTreeMap<Phase, Verified>>,
}
impl ForegroundConditions {
    pub fn new(staging: PathBuf, headless: bool) -> Self {
        Self { staging, headless, verified: Mutex::default() }
    }
}
impl Conditions for ForegroundConditions {
    fn prepare(&self, declaration: &Declaration, phase: Phase) -> anyhow::Result<()> {
        if phase == Phase::Post && !self.headless {
            bail!("post requires --headless; Zellij/TUI signals are not wired yet");
        }
        let mut verified = self.verified.lock().unwrap();
        for candidate in [Phase::Pre, Phase::Post] {
            if candidate > phase {
                continue;
            }
            if let Some(artifacts) = verify_phase(declaration, &self.staging, candidate)? {
                verified.insert(candidate, Arc::new(artifacts));
            }
        }
        Ok(())
    }
    fn wait(&self, condition: &str, progress: &dyn Fn(&str)) -> anyhow::Result<()> {
        if condition == "interface-ready" {
            if !self.headless {
                bail!("interface-ready has not been signaled");
            }
            progress("Explicit headless gate released");
            return Ok(());
        }
        let phase = condition_phase(condition)?;
        if !self.verified.lock().unwrap().contains_key(&phase) {
            bail!("missing verified {} staging; unpack then retry", phase.name());
        }
        Ok(())
    }
    fn verified(&self, phase: Phase) -> Option<Verified> {
        self.verified.lock().unwrap().get(&phase).cloned()
    }
}

/// The phase an `artifact-ready:<phase>` condition is about.
pub fn condition_phase(condition: &str) -> anyhow::Result<Phase> {
    match condition {
        "artifact-ready:pre" => Ok(Phase::Pre),
        "artifact-ready:post" => Ok(Phase::Post),
        other => bail!("unknown external condition {other:?}"),
    }
}

/// Verifies `<staging>/<phase>` against `declaration`: its receipt binds
/// this launch and digest, its content matches, and it holds exactly the
/// declared artifacts of that phase. `None` when nothing is staged yet.
pub fn verify_phase(
    declaration: &Declaration,
    staging: &Path,
    phase: Phase,
) -> anyhow::Result<Option<Vec<(PathBuf, PackedArtifact)>>> {
    let path = staging.join(phase.name());
    if fs::symlink_metadata(&path).is_err() {
        return Ok(None);
    }
    let descriptor: BundleDescriptor = beams_artifacts::read_json(&path.join("receipt.json"))?;
    if descriptor.launch_id != declaration.launch_id
        || descriptor.plan_digest != declaration.plan_digest
        || descriptor.phase != phase
    {
        bail!("staging belongs to another launch");
    }
    let manifest = beams_artifacts::verify_staging(&path, &descriptor)?;
    let expected: BTreeMap<_, _> = declaration
        .plan
        .artifacts
        .iter()
        .filter(|a| a.phase == phase)
        .map(|a| (a.id.as_str(), a.directory))
        .collect();
    let actual: BTreeMap<_, _> = manifest
        .artifacts
        .iter()
        .map(|a| (a.id.as_str(), a.directory))
        .collect();
    if expected != actual {
        bail!("staging must contain exactly the declared {phase:?} artifacts");
    }
    let path = path.canonicalize()?;
    Ok(Some(manifest.artifacts.into_iter().map(|a| (path.clone(), a)).collect()))
}

pub struct Options {
    pub home: PathBuf,
    pub state: PathBuf,
    pub phase: Phase,
    pub retry_interrupted: bool,
}
struct Context {
    declaration: Declaration,
    options: Options,
    conditions: Arc<dyn Conditions>,
}
#[derive(Serialize, Deserialize)]
struct PhaseReceipt {
    launch_id: String,
    plan_digest: String,
    phase: Phase,
}
#[derive(Serialize, Deserialize)]
struct ScriptRecord {
    key: String,
    status: String,
}

/// Validates `declaration` and runs its `options.phase` nodes through the
/// executor with `resources` (the daemon's shared ones, or a fresh set for
/// the foreground CLI). External conditions are released by `conditions`.
pub fn run(
    declaration: Declaration,
    options: Options,
    resources: &executor::Resources,
    conditions: Arc<dyn Conditions>,
    on_event: &(dyn Fn(executor::Event) + Sync),
) -> anyhow::Result<bool> {
    validate(&declaration)?;
    if !options.home.is_absolute() {
        bail!("Beam HOME must be absolute");
    }
    let state = options.state.join(&declaration.launch_id);
    fs::create_dir_all(&state)?;
    beams_artifacts::set_mode(&state, 0o700)?;
    let _lock = crate::packages::FileLock::acquire(&state.join("run.lock"))?;
    if options.phase == Phase::Post {
        let pre: PhaseReceipt = beams_artifacts::read_json(&state.join("pre.json"))
            .context("pre must complete for this launch before post")?;
        if pre.launch_id != declaration.launch_id
            || pre.plan_digest != declaration.plan_digest
            || pre.phase != Phase::Pre
        {
            bail!("pre receipt belongs to another launch");
        }
    }
    conditions.prepare(&declaration, options.phase)?;
    let selected = options.phase;
    let context = Arc::new(Context {
        declaration,
        options,
        conditions,
    });
    let mut tasks: Vec<Box<dyn Task>> = Vec::new();
    for node in context
        .declaration
        .plan
        .nodes
        .iter()
        .filter(|n| n.phase == selected)
    {
        let native = if let Action::Builtin { template, params } = &node.action {
            let params: BTreeMap<_, _> = params
                .iter()
                .map(|(key, value)| {
                    let value = match value {
                        ManifestParam::Str(s) => ManifestParam::Str(context.expand(s)),
                        other => other.clone(),
                    };
                    (key.clone(), value)
                })
                .collect();
            Some(crate::tasks::instantiate(
                template,
                serde_json::to_value(params)?,
            )?)
        } else {
            None
        };
        tasks.push(Box::new(LaunchTask {
            node: node.clone(),
            context: Arc::clone(&context),
            native,
        }));
    }
    // Pre providers may be omitted in post only with that launch's verified
    // successful pre receipt. No capability is borrowed from unrelated runs.
    executor::plan(&tasks)?;
    // A failed or interrupted retry must not leave an older success receipt
    // authorizing the next phase.
    let receipt_path = state.join(format!("{}.json", selected.name()));
    if receipt_path.exists() {
        fs::remove_file(&receipt_path)?;
    }
    let ok = executor::run_tagged(
        tasks,
        resources,
        Some(&context.declaration.launch_id),
        on_event,
    )?;
    if ok {
        beams_artifacts::write_json(
            &state.join(format!("{}.json", selected.name())),
            &PhaseReceipt {
                launch_id: context.declaration.launch_id.clone(),
                plan_digest: context.declaration.plan_digest.clone(),
                phase: selected,
            },
        )?;
    }
    Ok(ok)
}

/// The checks `run` makes before any effect: declaration identity and the
/// plan's structure (including native template instantiation).
pub fn validate(declaration: &Declaration) -> anyhow::Result<()> {
    beams_artifacts::validate_declaration(declaration)?;
    validate_plan(&declaration.plan)
}

fn validate_plan(plan: &LaunchPlan) -> anyhow::Result<()> {
    let nodes: BTreeMap<_, _> = plan.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    if nodes.len() != plan.nodes.len() {
        bail!("duplicate launch node ID");
    }
    let artifacts: BTreeMap<_, _> = plan.artifacts.iter().map(|a| (a.id.as_str(), a)).collect();
    if artifacts.len() != plan.artifacts.len() {
        bail!("duplicate artifact ID");
    }
    for id in [
        "interface-ready",
        "artifact-ready:pre",
        "artifact-ready:post",
        "runtime-ready",
        "packages:pre",
        "packages:post",
    ] {
        if !nodes.contains_key(id) {
            bail!("launch is missing {id}");
        }
    }
    for node in &plan.nodes {
        if node.phase == Phase::Post
            && node.id != "interface-ready"
            && !node.after.iter().any(|d| d == "interface-ready")
        {
            bail!("every post node must require interface-ready");
        }
        for dependency in &node.after {
            let dependency = nodes
                .get(dependency.as_str())
                .context("unknown launch dependency")?;
            if node.phase == Phase::Pre && dependency.phase == Phase::Post {
                bail!("pre node depends on post");
            }
        }
        match &node.action {
            Action::Condition => {
                if ![
                    "interface-ready",
                    "artifact-ready:pre",
                    "artifact-ready:post",
                ]
                .contains(&node.id.as_str())
                {
                    bail!("unknown external condition");
                }
                let phase = if node.id == "artifact-ready:pre" {
                    Phase::Pre
                } else {
                    Phase::Post
                };
                if node.phase != phase {
                    bail!("condition in wrong phase");
                }
            }
            Action::Runtime => {
                if node.id != "runtime-ready" || node.phase != Phase::Pre {
                    bail!("invalid runtime node");
                }
            }
            Action::Packages { packages } => {
                if node.id != format!("packages:{}", node.phase.name()) {
                    bail!("only one package batch per phase is allowed");
                }
                for package in packages {
                    crate::packages::package_name(package)?;
                }
            }
            Action::File {
                artifact, target, ..
            }
            | Action::Script {
                artifact,
                entrypoint: target,
                ..
            } => {
                let artifact = artifacts
                    .get(artifact.as_str())
                    .context("missing artifact declaration")?;
                if artifact.phase > node.phase {
                    bail!("artifact phase inversion");
                }
                if !node
                    .after
                    .contains(&format!("artifact-ready:{}", artifact.phase.name()))
                {
                    bail!("artifact consumer must require its verified bundle");
                }
                if let Action::Script {
                    interpreter, check, ..
                } = &node.action
                {
                    beams_artifacts::safe_path(target)?;
                    if interpreter.first().is_none_or(|s| s.is_empty())
                        || check
                            .as_ref()
                            .is_some_and(|v| v.first().is_none_or(|s| s.is_empty()))
                    {
                        bail!("empty script interpreter/check");
                    }
                } else if target == "/" || target == "~" || target == "${beam.home}" {
                    bail!("file target must not replace a root");
                }
            }
            Action::Builtin { template, params } => {
                let catalog = crate::tasks::templates();
                let schema = catalog
                    .iter()
                    .find(|s| s.template == template)
                    .context("unknown native launch template")?;
                for key in params.keys() {
                    if !schema.params.iter().any(|p| p.name == key) {
                        bail!("unknown parameter {key} for {template}");
                    }
                }
                // Instantiate both phases before effects, including integer
                // range checks. Instantiation itself performs no task work.
                crate::tasks::instantiate(template, serde_json::to_value(params)?)?;
            }
        }
    }
    let interface = nodes["interface-ready"];
    if !matches!(interface.action, Action::Condition) {
        bail!("interface-ready must be an external condition");
    }
    for node in plan.nodes.iter().filter(|n| n.phase == Phase::Pre) {
        if !interface.after.contains(&node.id) {
            bail!("interface-ready must depend on all pre nodes");
        }
    }
    for (id, action) in [
        ("artifact-ready:pre", 0),
        ("artifact-ready:post", 0),
        ("runtime-ready", 1),
        ("packages:pre", 2),
        ("packages:post", 2),
    ] {
        let valid = match action {
            0 => matches!(nodes[id].action, Action::Condition),
            1 => matches!(nodes[id].action, Action::Runtime),
            _ => matches!(nodes[id].action, Action::Packages { .. }),
        };
        if !valid {
            bail!("reserved node {id} has wrong action");
        }
    }
    let mut done = BTreeSet::new();
    while done.len() < nodes.len() {
        let next = plan.nodes.iter().find(|n| {
            !done.contains(n.id.as_str()) && n.after.iter().all(|d| done.contains(d.as_str()))
        });
        if let Some(node) = next {
            done.insert(node.id.as_str());
        } else {
            bail!("launch dependency cycle");
        }
    }
    Ok(())
}

impl Context {
    fn expand(&self, value: &str) -> String {
        value
            .replace("${beam.home}", &self.options.home.to_string_lossy())
            .replace("${beam.launch_id}", &self.declaration.launch_id)
    }
    fn path(&self, value: &str) -> anyhow::Result<PathBuf> {
        let value = if value == "~" {
            self.options.home.display().to_string()
        } else if let Some(tail) = value.strip_prefix("~/") {
            self.options.home.join(tail).display().to_string()
        } else {
            self.expand(value)
        };
        let path = PathBuf::from(value);
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("remote path must be normalized and absolute");
        }
        Ok(path)
    }
    fn artifact(&self, id: &str) -> anyhow::Result<(PathBuf, PackedArtifact)> {
        let phase = self
            .declaration
            .plan
            .artifacts
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.phase)
            .with_context(|| format!("artifact {id} is not declared"))?;
        self.conditions
            .verified(phase)
            .and_then(|verified| verified.iter().find(|(_, a)| a.id == id).cloned())
            .with_context(|| format!("artifact {id} has not been verified"))
    }
    fn environment(&self) -> BTreeMap<String, String> {
        let mut env: BTreeMap<_, _> = self
            .declaration
            .plan
            .env
            .iter()
            .map(|(k, v)| (k.clone(), self.expand(v)))
            .collect();
        let configured = self
            .declaration
            .plan
            .shell
            .path
            .as_ref()
            .into_iter()
            .flatten()
            .map(|p| self.expand(p))
            .collect::<Vec<_>>();
        let inherited = env
            .get("PATH")
            .cloned()
            .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
        env.insert(
            "PATH".into(),
            configured
                .into_iter()
                .chain([inherited])
                .collect::<Vec<_>>()
                .join(":"),
        );
        env.entry("HOME".into())
            .or_insert_with(|| self.options.home.display().to_string());
        env
    }
}
struct LaunchTask {
    node: Node,
    context: Arc<Context>,
    native: Option<Box<dyn Task>>,
}
impl Task for LaunchTask {
    fn id(&self) -> String {
        format!(
            "launch:{}:{}",
            self.context.declaration.launch_id, self.node.id
        )
    }
    fn provides(&self) -> Vec<Capability> {
        vec![self.id().into()]
    }
    fn requires(&self) -> Vec<Capability> {
        self.node
            .after
            .iter()
            .filter(|id| {
                self.context
                    .declaration
                    .plan
                    .nodes
                    .iter()
                    .any(|n| &n.id == *id && n.phase == self.node.phase)
            })
            .map(|id| format!("launch:{}:{id}", self.context.declaration.launch_id).into())
            .collect()
    }
    fn exclusive_resources(&self) -> Vec<Capability> {
        let mut resources: Vec<_> = self
            .node
            .resources
            .iter()
            .map(|r| self.context.expand(r).into())
            .collect();
        if let Some(native) = &self.native {
            resources.extend(native.exclusive_resources());
        }
        resources
    }
    fn run(&self) -> anyhow::Result<Option<String>> {
        self.run_with_progress(&|_| {})
    }
    fn run_with_progress(&self, progress: &dyn Fn(&str)) -> anyhow::Result<Option<String>> {
        match &self.node.action {
            Action::Condition => {
                self.context.conditions.wait(&self.node.id, progress)?;
                Ok(None)
            }
            Action::Runtime => {
                for path in self
                    .context
                    .declaration
                    .plan
                    .shell
                    .path
                    .as_ref()
                    .into_iter()
                    .flatten()
                {
                    super::files::ensure_parents(&self.context.path(path)?)?;
                }
                Ok(None)
            }
            Action::Packages { packages } => {
                crate::packages::PackageManager::default().install(packages, progress)
            }
            Action::File {
                artifact,
                target,
                mode,
                overwrite,
            } => {
                let (staging, artifact) = self.context.artifact(artifact)?;
                let (staging, artifact) = (&staging, &artifact);
                let target = self.context.path(target)?;
                if target == self.context.options.home || target.parent().is_none() {
                    bail!("file target must not replace a root");
                }
                if [".ssh", ".claude"]
                    .iter()
                    .any(|name| target == self.context.options.home.join(name))
                {
                    let mergeable = match fs::symlink_metadata(&target) {
                        Ok(metadata) => metadata.is_dir(),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                        Err(error) => return Err(error.into()),
                    };
                    if !artifact.directory || !matches!(mode, FileMode::Copy) || !mergeable {
                        bail!("protected home directories support directory merges, not wholesale replacement");
                    }
                }
                super::files::install(artifact, staging, &target, *mode, *overwrite, progress)
            }
            Action::Script { .. } => self.script(progress),
            Action::Builtin { .. } => self.native.as_ref().unwrap().run_with_progress(progress),
        }
    }
}
impl LaunchTask {
    fn script(&self, progress: &dyn Fn(&str)) -> anyhow::Result<Option<String>> {
        let Action::Script {
            artifact,
            entrypoint,
            interpreter,
            args,
            working_dir,
            env,
            check,
            rerun,
        } = &self.node.action
        else {
            unreachable!()
        };
        let (staging, artifact) = self.context.artifact(artifact)?;
        let (staging, artifact) = (&staging, &artifact);
        let root = staging.join(&artifact.root);
        let script = if artifact.directory {
            root.join(entrypoint)
        } else {
            if root.file_name().and_then(|n| n.to_str()) != Some(entrypoint) {
                bail!("script entrypoint differs from file artifact");
            }
            root.clone()
        };
        let relative = script
            .strip_prefix(staging)?
            .to_str()
            .context("script path must be UTF-8")?;
        if !artifact
            .entries
            .iter()
            .any(|e| e.path == relative && matches!(e.entry, EntryKind::File { .. }))
        {
            bail!("script is not a declared regular entry");
        }
        let script_dir = script.parent().unwrap();
        let root = if artifact.directory {
            root.as_path()
        } else {
            script_dir
        };
        let cwd = match working_dir {
            None => script_dir.to_owned(),
            Some(path)
                if path.starts_with('/')
                    || path.starts_with('~')
                    || path.starts_with("${beam.home}") =>
            {
                self.context.path(path)?
            }
            Some(path) => script_dir.join(self.context.expand(path)),
        };
        let mut environment = self.context.environment();
        for (key, value) in env {
            environment.insert(key.clone(), self.context.expand(value));
        }
        environment.insert("BEAMS_SCRIPT_DIR".into(), script_dir.display().to_string());
        environment.insert("BEAMS_SCRIPT_ROOT".into(), root.display().to_string());
        environment.insert(
            "BEAMS_LAUNCH_ID".into(),
            self.context.declaration.launch_id.clone(),
        );
        if let Some(run) = executor::current_task_run() {
            environment.insert("BEAMS_TASK_RUN".into(), run);
        }
        // The Beam's agent relay, as for native tasks (`sshagent`): a hook
        // run by the daemon would otherwise inherit its pre-relay
        // environment. Profile/task env may still set its own.
        if let Some(sock) = crate::sshagent::discover_ssh_auth_sock() {
            environment.entry("SSH_AUTH_SOCK".into()).or_insert(sock);
        }
        // Exclude transport/session nonces and extracted generation paths.
        let key = beams_artifacts::json_digest(&(
            &self.node,
            &artifact.digest,
            &self.context.options.home,
            &self.context.declaration.plan.env,
            &self.context.declaration.plan.shell.path,
        ))?;
        let _history_lock = if matches!(rerun, Rerun::OnChange) {
            Some(crate::packages::FileLock::acquire(
                &self
                    .context
                    .options
                    .state
                    .join("script-success")
                    .join(format!("{key}.lock")),
            )?)
        } else {
            None
        };
        let state = self
            .context
            .options
            .state
            .join(&self.context.declaration.launch_id)
            .join("scripts")
            .join(format!(
                "{}.json",
                beams_artifacts::json_digest(&self.node.id)?
            ));
        let previous: Option<ScriptRecord> = if state.exists() {
            Some(beams_artifacts::read_json(&state)?)
        } else {
            None
        };
        let probe = || -> anyhow::Result<bool> {
            let check = check.as_ref().unwrap();
            let mut command = Command::new(self.context.expand(&check[0]));
            command
                .args(check[1..].iter().map(|a| self.context.expand(a)))
                .envs(&environment)
                .current_dir(&cwd);
            Ok(crate::commandlog::status(&mut command, progress)?.success())
        };
        let checked = if check.is_some() {
            Some(probe()?)
        } else {
            None
        };
        let history = self
            .context
            .options
            .state
            .join("script-success")
            .join(format!("{key}.json"));
        let success: Option<ScriptRecord> = if history.exists() {
            Some(beams_artifacts::read_json(&history)?)
        } else {
            None
        };
        if checked == Some(true)
            || (check.is_none()
                && (previous
                    .as_ref()
                    .is_some_and(|p| p.key == key && p.status == "ok")
                    || matches!(rerun, Rerun::OnChange)
                        && success.is_some_and(|p| p.key == key && p.status == "ok")))
        {
            beams_artifacts::write_json(
                &state,
                &ScriptRecord {
                    key,
                    status: "ok".into(),
                },
            )?;
            return Ok(None);
        }
        if check.is_none()
            && previous.is_some_and(|p| p.status == "started")
            && !self.context.options.retry_interrupted
        {
            bail!("unchecked script was interrupted; review effects then use --retry-interrupted");
        }
        beams_artifacts::write_json(
            &state,
            &ScriptRecord {
                key: key.clone(),
                status: "started".into(),
            },
        )?;
        let mut command = Command::new(self.context.expand(&interpreter[0]));
        command
            .args(interpreter[1..].iter().map(|a| self.context.expand(a)))
            .arg(&script)
            .args(args.iter().map(|a| self.context.expand(a)))
            .envs(&environment)
            .current_dir(&cwd);
        let result = (|| {
            crate::commandlog::run(&mut command, progress)?;
            if check.is_some() && !probe()? {
                bail!("hook exited successfully but its desired-state check still fails");
            }
            Ok(())
        })();
        beams_artifacts::write_json(
            &state,
            &ScriptRecord {
                key: key.clone(),
                status: if result.is_ok() { "ok" } else { "failed" }.into(),
            },
        )?;
        result?;
        if matches!(rerun, Rerun::OnChange) {
            beams_artifacts::write_json(
                &history,
                &ScriptRecord {
                    key,
                    status: "ok".into(),
                },
            )?;
        }
        Ok(Some(format!("ran {}", self.node.label)))
    }
}
