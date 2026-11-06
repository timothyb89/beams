use super::{files, runner};
use crate::executor::Event;
use beams_artifacts::{Selection, TemporaryDirectory};
use beams_protocol::launch::*;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct Fixture {
    temp: TemporaryDirectory,
    home: PathBuf,
    staging: PathBuf,
    state: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = TemporaryDirectory::new(&std::env::temp_dir()).unwrap();
        let home = temp.0.join("home");
        fs::create_dir(&home).unwrap();
        Self {
            staging: temp.0.join("stage"),
            state: temp.0.join("state"),
            temp,
            home,
        }
    }
    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.temp.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }
    fn stage(
        &self,
        launch: &Declaration,
        phase: Phase,
        sources: &BTreeMap<String, Selection>,
    ) -> BundleManifest {
        self.stage_at(launch, phase, sources, &self.staging)
    }
    fn stage_at(
        &self,
        launch: &Declaration,
        phase: Phase,
        sources: &BTreeMap<String, Selection>,
        staging: &Path,
    ) -> BundleManifest {
        let archive = self
            .temp
            .0
            .join(format!("archive-{}", uuid::Uuid::new_v4()));
        let desc =
            beams_artifacts::pack(launch, phase, sources, &archive, Compression::Gzip).unwrap();
        beams_artifacts::receive(&archive, &desc, &staging.join(phase.name())).unwrap()
    }
    fn run(
        &self,
        launch: &Declaration,
        phase: Phase,
        headless: bool,
        retry: bool,
        events: &Mutex<Vec<(String, String)>>,
    ) -> anyhow::Result<bool> {
        self.run_at(launch, phase, headless, retry, events, &self.staging)
    }
    fn run_at(
        &self,
        launch: &Declaration,
        phase: Phase,
        headless: bool,
        retry: bool,
        events: &Mutex<Vec<(String, String)>>,
        staging: &Path,
    ) -> anyhow::Result<bool> {
        runner::run(
            launch.clone(),
            runner::Options {
                home: self.home.clone(),
                staging: staging.to_owned(),
                state: self.state.clone(),
                phase,
                headless,
                retry_interrupted: retry,
            },
            &|event| {
                let (id, status) = match event {
                    Event::Start { task_id } => (task_id, "start"),
                    Event::Skip { task_id } => (task_id, "skip"),
                    Event::Ok { task_id, .. } => (task_id, "ok"),
                    Event::Error { task_id, .. } => (task_id, "error"),
                    Event::Blocked { task_id, .. } => (task_id, "blocked"),
                    Event::Progress { task_id, .. } => (task_id, "progress"),
                };
                events.lock().unwrap().push((id, status.into()));
            },
        )
    }
}
fn node(id: &str, phase: Phase, after: &[&str], action: Action) -> Node {
    Node {
        id: id.into(),
        label: id.into(),
        phase,
        after: after.iter().map(|s| s.to_string()).collect(),
        resources: vec![],
        action,
    }
}
fn artifact(id: &str, phase: Phase, directory: bool) -> Artifact {
    Artifact {
        id: id.into(),
        phase,
        directory,
        exclude: vec![],
        used_by: vec![],
    }
}
fn script(id: &str, phase: Phase, entry: &str, after: &[&str], rerun: Rerun) -> Node {
    node(
        id,
        phase,
        after,
        Action::Script {
            artifact: "hooks".into(),
            entrypoint: entry.into(),
            interpreter: vec!["/bin/sh".into()],
            args: vec![],
            working_dir: None,
            env: BTreeMap::new(),
            check: None,
            rerun,
        },
    )
}
fn declaration(artifacts: Vec<Artifact>, mut extra: Vec<Node>) -> Declaration {
    let mut nodes = vec![
        node("artifact-ready:pre", Phase::Pre, &[], Action::Condition),
        node(
            "runtime-ready",
            Phase::Pre,
            &["artifact-ready:pre"],
            Action::Runtime,
        ),
        node(
            "packages:pre",
            Phase::Pre,
            &["runtime-ready"],
            Action::Packages { packages: vec![] },
        ),
        node(
            "interface-ready",
            Phase::Post,
            &["artifact-ready:pre", "runtime-ready", "packages:pre"],
            Action::Condition,
        ),
        node(
            "artifact-ready:post",
            Phase::Post,
            &["interface-ready"],
            Action::Condition,
        ),
        node(
            "packages:post",
            Phase::Post,
            &["interface-ready", "runtime-ready"],
            Action::Packages { packages: vec![] },
        ),
    ];
    for node in &mut extra {
        node.after.push("runtime-ready".into());
        if node.phase == Phase::Pre {
            nodes[3].after.push(node.id.clone());
        } else {
            node.after.push("interface-ready".into());
        }
    }
    nodes.extend(extra);
    beams_artifacts::declaration(LaunchPlan {
        schema_version: 1,
        shell: Shell {
            program: Some("/bin/sh".into()),
            path: Some(vec!["${beam.home}/bin".into()]),
            change_login_shell: Some(false),
        },
        env: BTreeMap::from([("TEAM".into(), "fixture".into())]),
        artifacts,
        nodes,
    })
    .unwrap()
}
fn status(events: &Mutex<Vec<(String, String)>>, suffix: &str, status: &str) -> bool {
    events
        .lock()
        .unwrap()
        .iter()
        .any(|(id, s)| id.ends_with(suffix) && s == status)
}

#[test]
fn pre_post_files_helpers_environment_and_repeated_runs_follow_graph() {
    let f = Fixture::new();
    f.write(
        "hooks/pre.sh",
        "printf 'pre\\n' >> \"$HOME/events\"; touch \"$HOME/pre-ready\"",
    );
    f.write(
        "hooks/lib/common.sh",
        "test \"$TEAM\" = fixture; test -n \"$BEAMS_LAUNCH_ID\"; test -n \"$BEAMS_TASK_RUN\"",
    );
    f.write(
        "hooks/post.sh",
        ". \"$BEAMS_SCRIPT_ROOT/lib/common.sh\"\ncat \"$HOME/data/input\" >> \"$HOME/events\"",
    );
    f.write("data/input", "post\n");
    f.write("home/data/unrelated", "keep");
    let launch = declaration(
        vec![
            artifact("hooks", Phase::Pre, true),
            artifact("data", Phase::Post, true),
        ],
        vec![
            script(
                "tasks:pre",
                Phase::Pre,
                "pre.sh",
                &["artifact-ready:pre"],
                Rerun::Manual,
            ),
            node(
                "files:data",
                Phase::Post,
                &["artifact-ready:post"],
                Action::File {
                    artifact: "data".into(),
                    target: "${beam.home}/data".into(),
                    mode: FileMode::Copy,
                    overwrite: Overwrite::Deny,
                },
            ),
            script(
                "tasks:post",
                Phase::Post,
                "post.sh",
                &["artifact-ready:pre", "files:data", "tasks:pre"],
                Rerun::Manual,
            ),
        ],
    );
    let sources = BTreeMap::from([
        ("hooks".into(), Selection::Local(f.temp.0.join("hooks"))),
        ("data".into(), Selection::Local(f.temp.0.join("data"))),
    ]);
    f.stage(&launch, Phase::Pre, &sources);
    let events = Mutex::new(vec![]);
    assert!(f.run(&launch, Phase::Post, true, false, &events).is_err());
    assert!(f.run(&launch, Phase::Pre, false, false, &events).unwrap());
    assert_eq!(fs::read_to_string(f.home.join("events")).unwrap(), "pre\n");
    f.stage(&launch, Phase::Post, &sources);
    assert!(f.run(&launch, Phase::Post, false, false, &events).is_err());
    assert!(f.run(&launch, Phase::Post, true, false, &events).unwrap());
    assert_eq!(
        fs::read_to_string(f.home.join("events")).unwrap(),
        "pre\npost\n"
    );
    assert_eq!(
        fs::read_to_string(f.home.join("data/unrelated")).unwrap(),
        "keep"
    );
    assert!(f.run(&launch, Phase::Pre, false, false, &events).unwrap());
    assert!(f.run(&launch, Phase::Post, true, false, &events).unwrap());
    assert_eq!(
        fs::read_to_string(f.home.join("events")).unwrap(),
        "pre\npost\n"
    );
    assert!(status(&events, "tasks:post", "skip"));
}

#[test]
fn failures_block_dependents_and_pre_completion_but_independent_tasks_continue() {
    let f = Fixture::new();
    f.write("hooks/fail.sh", "exit 6");
    f.write("hooks/dependent.sh", "touch \"$HOME/never\"");
    f.write("hooks/free.sh", "touch \"$HOME/free\"");
    let launch = declaration(
        vec![artifact("hooks", Phase::Pre, true)],
        vec![
            script(
                "tasks:fail",
                Phase::Pre,
                "fail.sh",
                &["artifact-ready:pre"],
                Rerun::Manual,
            ),
            script(
                "tasks:dependent",
                Phase::Pre,
                "dependent.sh",
                &["tasks:fail", "artifact-ready:pre"],
                Rerun::Manual,
            ),
            script(
                "tasks:free",
                Phase::Pre,
                "free.sh",
                &["artifact-ready:pre"],
                Rerun::Manual,
            ),
        ],
    );
    f.stage(
        &launch,
        Phase::Pre,
        &BTreeMap::from([("hooks".into(), Selection::Local(f.temp.0.join("hooks")))]),
    );
    let events = Mutex::new(vec![]);
    assert!(!f.run(&launch, Phase::Pre, false, false, &events).unwrap());
    assert!(f.home.join("free").exists() && !f.home.join("never").exists());
    assert!(status(&events, "tasks:dependent", "blocked"));
    assert!(!f.state.join(&launch.launch_id).join("pre.json").exists());
    assert!(f.run(&launch, Phase::Post, true, false, &events).is_err());
}

#[test]
fn missing_post_artifact_only_blocks_its_dependents_and_retry_skips_completed_hooks() {
    let f = Fixture::new();
    f.write("hooks/free.sh", "printf 'once\\n' >> \"$HOME/events\"");
    f.write("data/file", "data");
    let launch = declaration(
        vec![
            artifact("hooks", Phase::Pre, true),
            artifact("data", Phase::Post, true),
        ],
        vec![
            script(
                "tasks:free",
                Phase::Post,
                "free.sh",
                &["artifact-ready:pre"],
                Rerun::Manual,
            ),
            node(
                "files:data",
                Phase::Post,
                &["artifact-ready:post"],
                Action::File {
                    artifact: "data".into(),
                    target: "${beam.home}/data".into(),
                    mode: FileMode::Copy,
                    overwrite: Overwrite::Deny,
                },
            ),
        ],
    );
    let sources = BTreeMap::from([
        ("hooks".into(), Selection::Local(f.temp.0.join("hooks"))),
        ("data".into(), Selection::Local(f.temp.0.join("data"))),
    ]);
    f.stage(&launch, Phase::Pre, &sources);
    let events = Mutex::new(vec![]);
    assert!(f.run(&launch, Phase::Pre, false, false, &events).unwrap());
    assert!(!f.run(&launch, Phase::Post, true, false, &events).unwrap());
    assert!(status(&events, "files:data", "blocked"));
    assert_eq!(fs::read_to_string(f.home.join("events")).unwrap(), "once\n");
    f.stage(&launch, Phase::Post, &sources);
    assert!(f.run(&launch, Phase::Post, true, false, &events).unwrap());
    assert_eq!(fs::read_to_string(f.home.join("events")).unwrap(), "once\n");
}

#[test]
fn on_change_history_is_content_based_across_generations_and_helpers_count() {
    let f = Fixture::new();
    f.write("hooks/run.sh", "printf 'run\\n' >> \"$HOME/events\"");
    f.write("hooks/helper", "version 1");
    let sources = BTreeMap::from([("hooks".into(), Selection::Local(f.temp.0.join("hooks")))]);
    let launch = declaration(
        vec![artifact("hooks", Phase::Pre, true)],
        vec![script(
            "tasks:run",
            Phase::Pre,
            "run.sh",
            &["artifact-ready:pre"],
            Rerun::OnChange,
        )],
    );
    let events = Mutex::new(vec![]);
    for index in 0..3 {
        if index == 2 {
            f.write("hooks/helper", "version 2");
        }
        let next = beams_artifacts::declaration(launch.plan.clone()).unwrap();
        let stage = f.temp.0.join(format!("stage-{index}"));
        f.stage_at(&next, Phase::Pre, &sources, &stage);
        assert!(f
            .run_at(&next, Phase::Pre, false, false, &events, &stage)
            .unwrap());
    }
    assert_eq!(
        fs::read_to_string(f.home.join("events")).unwrap(),
        "run\nrun\n"
    );
}

#[test]
fn unchecked_interrupted_hooks_require_explicit_retry_and_checks_repair_drift() {
    let f = Fixture::new();
    f.write("hooks/run.sh", "touch \"$HOME/ready\"");
    let sources = BTreeMap::from([("hooks".into(), Selection::Local(f.temp.0.join("hooks")))]);
    let mut task = script(
        "tasks:run",
        Phase::Pre,
        "run.sh",
        &["artifact-ready:pre"],
        Rerun::Manual,
    );
    let launch = declaration(
        vec![artifact("hooks", Phase::Pre, true)],
        vec![task.clone()],
    );
    f.stage(&launch, Phase::Pre, &sources);
    let record = f
        .state
        .join(&launch.launch_id)
        .join("scripts")
        .join(format!(
            "{}.json",
            beams_artifacts::json_digest(&"tasks:run").unwrap()
        ));
    beams_artifacts::write_json(
        &record,
        &serde_json::json!({"key":"interrupted", "status":"started"}),
    )
    .unwrap();
    let events = Mutex::new(vec![]);
    assert!(!f.run(&launch, Phase::Pre, false, false, &events).unwrap());
    assert!(!f.home.join("ready").exists());
    assert!(f.run(&launch, Phase::Pre, false, true, &events).unwrap());
    let Action::Script { check, .. } = &mut task.action else {
        unreachable!()
    };
    *check = Some(vec![
        "test".into(),
        "-f".into(),
        "${beam.home}/ready".into(),
    ]);
    let next = declaration(vec![artifact("hooks", Phase::Pre, true)], vec![task]);
    let stage = f.temp.0.join("checked-stage");
    f.stage_at(&next, Phase::Pre, &sources, &stage);
    assert!(f
        .run_at(&next, Phase::Pre, false, false, &events, &stage)
        .unwrap());
    fs::remove_file(f.home.join("ready")).unwrap();
    assert!(f
        .run_at(&next, Phase::Pre, false, false, &events, &stage)
        .unwrap());
    assert!(f.home.join("ready").exists());
}

#[test]
fn file_policies_preserve_backups_modes_internal_links_and_parent_boundaries() {
    let f = Fixture::new();
    let input = f.write("source/input", "new");
    beams_artifacts::set_mode(&input, 0o751).unwrap();
    let launch = declaration(vec![artifact("file", Phase::Pre, false)], vec![]);
    let manifest = f.stage(
        &launch,
        Phase::Pre,
        &BTreeMap::from([("file".into(), Selection::Local(input))]),
    );
    let file = &manifest.artifacts[0];
    let stage = f.staging.join("pre");
    let target = f.write("home/target", "old");
    assert!(files::install(
        file,
        &stage,
        &target,
        FileMode::Copy,
        Overwrite::Deny,
        &|_| {}
    )
    .is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "old");
    files::install(
        file,
        &stage,
        &target,
        FileMode::Copy,
        Overwrite::Backup,
        &|_| {},
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    let backups: Vec<_> = fs::read_dir(&f.home)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".beams-backup-"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read_to_string(backups[0].path()).unwrap(), "old");
    assert!(files::install(
        file,
        &stage,
        &target,
        FileMode::Copy,
        Overwrite::Backup,
        &|_| {}
    )
    .unwrap()
    .is_none());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o751
    );
    fs::create_dir(f.temp.0.join("outside")).unwrap();
    std::os::unix::fs::symlink(f.temp.0.join("outside"), f.home.join("escape")).unwrap();
    assert!(files::install(
        file,
        &stage,
        &f.home.join("escape/target"),
        FileMode::Copy,
        Overwrite::Replace,
        &|_| {}
    )
    .is_err());
    assert!(!f.temp.0.join("outside/target").exists());
    files::install(
        file,
        &stage,
        &f.home.join("linked"),
        FileMode::Link,
        Overwrite::Deny,
        &|_| {},
    )
    .unwrap();
    assert_eq!(
        fs::read_link(f.home.join("linked")).unwrap(),
        stage.join(&file.root)
    );

    f.write("tree/nested/file", "tree");
    fs::create_dir(f.temp.0.join("tree/empty")).unwrap();
    std::os::unix::fs::symlink("nested/file", f.temp.0.join("tree/link")).unwrap();
    let tree_launch = declaration(vec![artifact("tree", Phase::Pre, true)], vec![]);
    let tree_stage = f.temp.0.join("tree-stage");
    let tree_manifest = f.stage_at(
        &tree_launch,
        Phase::Pre,
        &BTreeMap::from([("tree".into(), Selection::Local(f.temp.0.join("tree")))]),
        &tree_stage,
    );
    let destination = f.home.join("tree");
    files::install(
        &tree_manifest.artifacts[0],
        &tree_stage.join("pre"),
        &destination,
        FileMode::Copy,
        Overwrite::Deny,
        &|_| {},
    )
    .unwrap();
    assert!(destination.join("empty").is_dir());
    assert_eq!(
        fs::read_link(destination.join("link")).unwrap(),
        Path::new("nested/file")
    );
    assert_eq!(
        fs::read_to_string(destination.join("link")).unwrap(),
        "tree"
    );
}

#[test]
fn failed_pre_retry_revokes_previous_success_before_post_can_run() {
    let f = Fixture::new();
    f.write(
        "hooks/run.sh",
        "test ! -f \"$HOME/fail\" || exit 8\ntouch \"$HOME/ready\"",
    );
    let mut task = script(
        "tasks:run",
        Phase::Pre,
        "run.sh",
        &["artifact-ready:pre"],
        Rerun::Manual,
    );
    let Action::Script { check, .. } = &mut task.action else {
        unreachable!()
    };
    *check = Some(vec![
        "test".into(),
        "-f".into(),
        "${beam.home}/ready".into(),
    ]);
    let launch = declaration(vec![artifact("hooks", Phase::Pre, true)], vec![task]);
    f.stage(
        &launch,
        Phase::Pre,
        &BTreeMap::from([("hooks".into(), Selection::Local(f.temp.0.join("hooks")))]),
    );
    let events = Mutex::new(vec![]);
    assert!(f.run(&launch, Phase::Pre, false, false, &events).unwrap());
    let receipt = f.state.join(&launch.launch_id).join("pre.json");
    assert!(receipt.exists());
    fs::remove_file(f.home.join("ready")).unwrap();
    f.write("home/fail", "");
    assert!(!f.run(&launch, Phase::Pre, false, false, &events).unwrap());
    assert!(!receipt.exists());
    assert!(f.run(&launch, Phase::Post, true, false, &events).is_err());
}

#[test]
fn malformed_post_plans_are_rejected_before_pre_effects() {
    let f = Fixture::new();
    f.write("hooks/run.sh", "touch \"$HOME/effect\"");
    let base = declaration(
        vec![artifact("hooks", Phase::Pre, true)],
        vec![script(
            "tasks:run",
            Phase::Pre,
            "run.sh",
            &["artifact-ready:pre"],
            Rerun::Manual,
        )],
    );
    let events = Mutex::new(vec![]);
    for case in 0..4 {
        let mut plan = base.plan.clone();
        match case {
            0 => {
                let post = plan
                    .nodes
                    .iter_mut()
                    .find(|n| n.id == "packages:post")
                    .unwrap();
                post.after.push("artifact-ready:post".into());
                let ready = plan
                    .nodes
                    .iter_mut()
                    .find(|n| n.id == "artifact-ready:post")
                    .unwrap();
                ready.after.push("packages:post".into());
            }
            1 => plan.nodes.push(node(
                "tasks:unknown",
                Phase::Post,
                &["interface-ready"],
                Action::Builtin {
                    template: "unknown-template".into(),
                    params: BTreeMap::new(),
                },
            )),
            2 => plan.nodes.push(node(
                "tasks:bad-params",
                Phase::Post,
                &["interface-ready"],
                Action::Builtin {
                    template: "clone-git-repo".into(),
                    params: BTreeMap::new(),
                },
            )),
            _ => {
                let pre = plan.nodes.iter_mut().find(|n| n.id == "tasks:run").unwrap();
                pre.after.retain(|d| d != "artifact-ready:pre");
            }
        }
        let launch = beams_artifacts::declaration(plan).unwrap();
        let stage = f.temp.0.join(format!("bad-stage-{case}"));
        f.stage_at(
            &launch,
            Phase::Pre,
            &BTreeMap::from([("hooks".into(), Selection::Local(f.temp.0.join("hooks")))]),
            &stage,
        );
        assert!(f
            .run_at(&launch, Phase::Pre, false, false, &events, &stage)
            .is_err());
        assert!(!f.home.join("effect").exists());
    }
}

#[test]
fn protected_home_directories_merge_but_cannot_be_replaced_wholesale() {
    let f = Fixture::new();
    f.write("configs/new", "selected");
    f.write("home/.ssh/identity", "Beam-managed");
    let sources = BTreeMap::from([("configs".into(), Selection::Local(f.temp.0.join("configs")))]);
    let make_launch = |mode| {
        declaration(
            vec![artifact("configs", Phase::Pre, true)],
            vec![node(
                "files:configs",
                Phase::Pre,
                &["artifact-ready:pre"],
                Action::File {
                    artifact: "configs".into(),
                    target: "${beam.home}/.ssh".into(),
                    mode,
                    overwrite: Overwrite::Backup,
                },
            )],
        )
    };
    let events = Mutex::new(vec![]);
    let linked = make_launch(FileMode::Link);
    f.stage(&linked, Phase::Pre, &sources);
    assert!(!f.run(&linked, Phase::Pre, false, false, &events).unwrap());
    assert_eq!(
        fs::read_to_string(f.home.join(".ssh/identity")).unwrap(),
        "Beam-managed"
    );
    let copied = make_launch(FileMode::Copy);
    let stage = f.temp.0.join("merge-stage");
    f.stage_at(&copied, Phase::Pre, &sources, &stage);
    assert!(f
        .run_at(&copied, Phase::Pre, false, false, &events, &stage)
        .unwrap());
    assert_eq!(
        fs::read_to_string(f.home.join(".ssh/identity")).unwrap(),
        "Beam-managed"
    );
    assert_eq!(
        fs::read_to_string(f.home.join(".ssh/new")).unwrap(),
        "selected"
    );
}
