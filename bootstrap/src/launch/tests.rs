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
                state: self.state.clone(),
                phase,
                retry_interrupted: retry,
            },
            &crate::executor::Resources::default(),
            std::sync::Arc::new(runner::ForegroundConditions::new(staging.to_owned(), headless)),
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

// --- Daemon-accepted launches (`launch::daemon`) ---------------------------

mod daemon_launches {
    use super::*;
    use crate::launch::daemon::{handle, Host, LaunchSnapshot, LaunchStatus, Launches, NodeStatus};
    use serde_json::{json, Value};
    use std::sync::{Arc, Condvar};
    use std::time::{Duration, Instant};

    /// A daemon stand-in: shared resources, a launch table, and one ordered
    /// log of everything a single connection would receive (broadcasts and
    /// direct replies alike).
    struct FakeHost {
        resources: crate::executor::Resources,
        launches: Launches,
        received: Mutex<Vec<Value>>,
        changed: Condvar,
    }
    impl Host for FakeHost {
        fn broadcast(&self, line: &str) {
            self.received.lock().unwrap().push(serde_json::from_str(line).unwrap());
            self.changed.notify_all();
        }
        fn resources(&self) -> &crate::executor::Resources {
            &self.resources
        }
        fn launches(&self) -> &Launches {
            &self.launches
        }
    }

    fn host(f: &Fixture) -> Arc<FakeHost> {
        let launches = Launches::new(f.state.clone(), f.home.clone());
        launches.load();
        Arc::new(FakeHost {
            resources: Default::default(),
            launches,
            received: Mutex::default(),
            changed: Condvar::new(),
        })
    }

    /// Runs one op; its replies are returned and also appended to the
    /// connection log, in order with broadcasts.
    fn call(host: &Arc<FakeHost>, op: &str, request: Value) -> Value {
        let replies = Mutex::new(Vec::new());
        handle(host, op, &request, &|reply| {
            host.received.lock().unwrap().push(reply.clone());
            replies.lock().unwrap().push(reply);
        });
        let mut replies = replies.into_inner().unwrap();
        assert_eq!(replies.len(), 1, "one reply per op: {replies:?}");
        replies.pop().unwrap()
    }

    fn write_launch(f: &Fixture, launch: &Declaration) -> PathBuf {
        let path = f.temp.0.join(format!("launch-{}.json", launch.launch_id));
        beams_artifacts::write_json(&path, launch).unwrap();
        path
    }

    fn submit(f: &Fixture, host: &Arc<FakeHost>, launch: &Declaration, gate: &str) -> Value {
        call(
            host,
            "launch_submit",
            json!({"launch": write_launch(f, launch), "staging": f.staging, "gate": gate}),
        )
    }

    fn snapshot(host: &Arc<FakeHost>, id: &str) -> LaunchSnapshot {
        let reply = call(host, "launch_snapshot", json!({"launch_id": id}));
        serde_json::from_value(reply["launches"][0].clone()).unwrap()
    }

    fn node_status(snapshot: &LaunchSnapshot, id: &str) -> NodeStatus {
        snapshot.nodes.iter().find(|n| n.id == id).unwrap().status
    }

    /// Polls the launch until `done` holds (5s at most).
    fn wait_until(host: &Arc<FakeHost>, id: &str, what: &str, done: impl Fn(&LaunchSnapshot) -> bool) -> LaunchSnapshot {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let current = snapshot(host, id);
            if done(&current) {
                return current;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {current:#?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn finished(snapshot: &LaunchSnapshot) -> bool {
        matches!(snapshot.status, LaunchStatus::Ok | LaunchStatus::Failed)
    }

    fn hooks(f: &Fixture) -> BTreeMap<String, Selection> {
        BTreeMap::from([("hooks".into(), Selection::Local(f.temp.0.join("hooks")))])
    }

    fn pre_post_launch(f: &Fixture) -> Declaration {
        f.write("hooks/pre.sh", "printf 'pre\\n' >> \"$HOME/events\"");
        f.write("hooks/post.sh", "printf 'post\\n' >> \"$HOME/events\"");
        declaration(
            vec![artifact("hooks", Phase::Pre, true)],
            vec![
                script("tasks:pre", Phase::Pre, "pre.sh", &["artifact-ready:pre"], Rerun::Manual),
                script("tasks:post", Phase::Post, "post.sh", &["artifact-ready:pre", "tasks:pre"], Rerun::Manual),
            ],
        )
    }

    #[test]
    fn accepted_launch_waits_for_its_staging_and_interface_then_runs_both_phases() {
        let f = Fixture::new();
        let launch = pre_post_launch(&f);
        let id = launch.launch_id.clone();
        let host = host(&f);

        let accepted = submit(&f, &host, &launch, "interactive");
        assert_eq!(accepted["type"], "launch_accepted", "{accepted}");
        assert_eq!(accepted["existing"], false);
        let state = wait_until(&host, &id, "the pre bundle wait", |s| {
            node_status(s, "artifact-ready:pre") == NodeStatus::Waiting
        });
        assert_eq!(state.status, LaunchStatus::Running);
        assert!(!f.home.join("events").exists(), "nothing runs before its bundle");

        // An identical resubmission is the same launch; a different plan
        // under the same ID is refused before any effect.
        let again = submit(&f, &host, &launch, "interactive");
        assert_eq!(again["existing"], true);
        let mut conflicting = pre_post_launch(&f);
        conflicting.plan.env.insert("EXTRA".into(), "1".into());
        conflicting.plan_digest = beams_artifacts::json_digest(&conflicting.plan).unwrap();
        conflicting.launch_id = id.clone();
        let refused = submit(&f, &host, &conflicting, "interactive");
        assert_eq!(refused["type"], "error");
        assert!(refused["message"].as_str().unwrap().contains("different plan digest"), "{refused}");

        // Signalling before anything is staged doesn't release it.
        let early = call(&host, "launch_signal", json!({"launch_id": id, "condition": "artifact-ready:pre"}));
        assert_eq!(early["released"], false, "{early}");

        f.stage(&launch, Phase::Pre, &hooks(&f));
        let released = call(&host, "launch_signal", json!({"launch_id": id, "condition": "artifact-ready:pre"}));
        assert_eq!(released["released"], true, "{released}");
        let state = wait_until(&host, &id, "the interface wait", |s| {
            node_status(s, "interface-ready") == NodeStatus::Waiting
        });
        assert_eq!(fs::read_to_string(f.home.join("events")).unwrap(), "pre\n");
        assert_eq!(node_status(&state, "tasks:pre"), NodeStatus::Ok);
        assert_eq!(node_status(&state, "tasks:post"), NodeStatus::Pending, "post waits for the interface");

        // Post work that only needs pre artifacts proceeds while the post
        // bundle is still on its way.
        call(&host, "launch_signal", json!({"launch_id": id, "condition": "interface-ready"}));
        let state = wait_until(&host, &id, "post progress without its bundle", |s| {
            node_status(s, "tasks:post") == NodeStatus::Ok
        });
        assert_eq!(node_status(&state, "artifact-ready:post"), NodeStatus::Waiting);
        assert_eq!(state.status, LaunchStatus::Running);
        f.stage(&launch, Phase::Post, &hooks(&f));
        call(&host, "launch_signal", json!({"launch_id": id, "condition": "artifact-ready:post"}));
        let state = wait_until(&host, &id, "completion", finished);
        assert_eq!(state.status, LaunchStatus::Ok, "{state:#?}");
        assert_eq!(fs::read_to_string(f.home.join("events")).unwrap(), "pre\npost\n");
        assert!(state.nodes.iter().all(|n| matches!(n.status, NodeStatus::Ok | NodeStatus::Skip)), "{state:#?}");

        // A finished launch can't be retried; a resubmission still answers.
        let retry = call(&host, "launch_retry", json!({"launch_id": id}));
        assert_eq!(retry["type"], "error");
        assert_eq!(submit(&f, &host, &launch, "interactive")["existing"], true);
    }

    #[test]
    fn a_snapshot_is_an_exact_boundary_in_the_event_stream() {
        let f = Fixture::new();
        f.write("hooks/pre.sh", "echo started; while [ ! -e \"$HOME/go\" ]; do sleep 0.02; done; echo finishing");
        let launch = declaration(
            vec![artifact("hooks", Phase::Pre, true)],
            vec![script("tasks:pre", Phase::Pre, "pre.sh", &["artifact-ready:pre"], Rerun::Manual)],
        );
        let id = launch.launch_id.clone();
        f.stage(&launch, Phase::Pre, &hooks(&f));
        f.stage(&launch, Phase::Post, &hooks(&f));
        let host = host(&f);
        submit(&f, &host, &launch, "headless");
        let middle = wait_until(&host, &id, "the hook to run", |s| {
            s.nodes.iter().any(|n| n.id == "tasks:pre" && n.log.iter().any(|l| l.contains("started")))
        });
        assert_eq!(node_status(&middle, "tasks:pre"), NodeStatus::Running);
        fs::write(f.home.join("go"), "").unwrap();
        let last = wait_until(&host, &id, "completion", finished);
        assert_eq!(last.status, LaunchStatus::Ok, "{last:#?}");
        assert!(last.nodes.iter().find(|n| n.id == "tasks:pre").unwrap().log.iter().any(|l| l.contains("finishing")));

        // Every snapshot reply sits exactly between the events it includes
        // and the ones it doesn't; event numbers only grow.
        let received = host.received.lock().unwrap().clone();
        let mut previous = 0;
        let boundary_reply = |m: &Value| ["launch_snapshot", "launch_accepted", "launch_retrying"].contains(&m["type"].as_str().unwrap_or_default());
        let events = |range: &[Value]| -> Vec<u64> {
            range.iter().filter(|m| !boundary_reply(m)).filter_map(|m| m["seq"].as_u64()).collect()
        };
        assert!(received.iter().filter(|m| boundary_reply(m)).count() >= 3, "accept plus polling snapshots");
        for (index, message) in received.iter().enumerate() {
            if boundary_reply(message) {
                let boundary = message["seq"].as_u64().unwrap();
                assert!(events(&received[..index]).iter().all(|seq| *seq <= boundary), "{message}");
                assert!(events(&received[index + 1..]).iter().all(|seq| *seq > boundary), "{message}");
            } else if let Some(seq) = message["seq"].as_u64() {
                assert!(seq > previous, "seq must increase: {message}");
                previous = seq;
            }
        }
        // Broadcast task events keep the shape the Beam TUI renders.
        assert!(received.iter().any(|m| m["type"] == "task_status"
            && m["status"] == "ok"
            && m["manifest_id"] == id.as_str()
            && m["node"] == "tasks:pre"));
    }

    #[test]
    fn a_pre_failure_blocks_post_and_an_explicit_retry_resumes() {
        let f = Fixture::new();
        f.write("hooks/pre.sh", "test -e \"$HOME/fixed\"");
        f.write("hooks/post.sh", "touch \"$HOME/post-ran\"");
        let launch = declaration(
            vec![artifact("hooks", Phase::Pre, true)],
            vec![
                script("tasks:pre", Phase::Pre, "pre.sh", &["artifact-ready:pre"], Rerun::Manual),
                script("tasks:post", Phase::Post, "post.sh", &["artifact-ready:pre"], Rerun::Manual),
            ],
        );
        let id = launch.launch_id.clone();
        f.stage(&launch, Phase::Pre, &hooks(&f));
        f.stage(&launch, Phase::Post, &hooks(&f));
        let host = host(&f);
        submit(&f, &host, &launch, "headless");
        let failed = wait_until(&host, &id, "failure", finished);
        assert_eq!(failed.status, LaunchStatus::Failed);
        assert_eq!(node_status(&failed, "tasks:pre"), NodeStatus::Error);
        assert_eq!(node_status(&failed, "tasks:post"), NodeStatus::Blocked);
        assert_eq!(node_status(&failed, "interface-ready"), NodeStatus::Blocked);
        assert!(!f.home.join("post-ran").exists());

        fs::write(f.home.join("fixed"), "").unwrap();
        let retrying = call(&host, "launch_retry", json!({"launch_id": id}));
        assert_eq!(retrying["type"], "launch_retrying", "{retrying}");
        let done = wait_until(&host, &id, "the retry", finished);
        assert_eq!(done.status, LaunchStatus::Ok, "{done:#?}");
        assert!(f.home.join("post-ran").exists());
    }

    #[test]
    fn staging_that_fails_verification_fails_its_condition() {
        let f = Fixture::new();
        let launch = pre_post_launch(&f);
        let id = launch.launch_id.clone();
        let manifest = f.stage(&launch, Phase::Pre, &hooks(&f));
        // Tamper with a staged (read-only) hook.
        let staged = f.staging.join("pre").join(&manifest.artifacts[0].root).join("pre.sh");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(staged.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&staged, "touch \"$HOME/tampered\"").unwrap();

        let host = host(&f);
        submit(&f, &host, &launch, "headless");
        let failed = wait_until(&host, &id, "failure", finished);
        assert_eq!(failed.status, LaunchStatus::Failed);
        let ready = failed.nodes.iter().find(|n| n.id == "artifact-ready:pre").unwrap();
        assert_eq!(ready.status, NodeStatus::Error);
        assert!(ready.message.as_deref().unwrap().contains("failed verification"), "{ready:?}");
        assert_eq!(node_status(&failed, "tasks:pre"), NodeStatus::Blocked);
        assert!(!f.home.join("tampered").exists() && !f.home.join("events").exists());
    }

    #[test]
    fn launches_sharing_a_resource_never_overlap_on_the_daemon() {
        let f = Fixture::new();
        f.write(
            "hooks/locked.sh",
            "date +%s%N >> \"$HOME/intervals\"; sleep 0.3; date +%s%N >> \"$HOME/intervals\"",
        );
        let make = || {
            let mut locked = script("tasks:locked", Phase::Pre, "locked.sh", &["artifact-ready:pre"], Rerun::Manual);
            locked.resources = vec!["fixture-shared".into()];
            declaration(vec![artifact("hooks", Phase::Pre, true)], vec![locked])
        };
        let (first, second) = (make(), make());
        let host = host(&f);
        let mut stagings = Vec::new();
        for launch in [&first, &second] {
            let staging = f.temp.0.join(format!("stage-{}", launch.launch_id));
            f.stage_at(launch, Phase::Pre, &hooks(&f), &staging);
            f.stage_at(launch, Phase::Post, &hooks(&f), &staging);
            stagings.push(staging);
        }
        for (launch, staging) in [&first, &second].into_iter().zip(&stagings) {
            let reply = call(
                &host,
                "launch_submit",
                json!({"launch": write_launch(&f, launch), "staging": staging, "gate": "headless"}),
            );
            assert_eq!(reply["type"], "launch_accepted", "{reply}");
        }
        for launch in [&first, &second] {
            let done = wait_until(&host, &launch.launch_id, "completion", finished);
            assert_eq!(done.status, LaunchStatus::Ok, "{done:#?}");
        }
        let times: Vec<u128> = fs::read_to_string(f.home.join("intervals"))
            .unwrap()
            .lines()
            .map(|l| l.parse().unwrap())
            .collect();
        assert_eq!(times.len(), 4);
        // start, end, start, end: the second started after the first ended.
        assert!(times[0] < times[1] && times[1] <= times[2] && times[2] < times[3], "{times:?}");
    }

    #[test]
    fn a_daemon_restart_leaves_running_launches_interrupted_until_retried() {
        let f = Fixture::new();
        f.write("hooks/pre.sh", "while [ ! -e \"$HOME/go\" ]; do sleep 0.02; done; printf 'pre\\n' >> \"$HOME/events\"");
        let launch = declaration(
            vec![artifact("hooks", Phase::Pre, true)],
            vec![script("tasks:pre", Phase::Pre, "pre.sh", &["artifact-ready:pre"], Rerun::Manual)],
        );
        let id = launch.launch_id.clone();
        f.stage(&launch, Phase::Pre, &hooks(&f));
        f.stage(&launch, Phase::Post, &hooks(&f));
        let before = host(&f);
        submit(&f, &before, &launch, "headless");
        wait_until(&before, &id, "the hook to start", |s| node_status(s, "tasks:pre") == NodeStatus::Running);

        // A new daemon over the same state.
        let after = host(&f);
        let state = snapshot(&after, &id);
        assert_eq!(state.status, LaunchStatus::Interrupted);
        assert_eq!(node_status(&state, "tasks:pre"), NodeStatus::Interrupted);
        assert_eq!(submit(&f, &after, &launch, "headless")["existing"], true, "resubmission doesn't restart it");

        // Let the old run finish (its hook is still ours), then resume.
        fs::write(f.home.join("go"), "").unwrap();
        wait_until(&before, &id, "the old run", finished);
        let retrying = call(&after, "launch_retry", json!({"launch_id": id}));
        assert_eq!(retrying["type"], "launch_retrying", "{retrying}");
        let done = wait_until(&after, &id, "the resumed run", finished);
        assert_eq!(done.status, LaunchStatus::Ok, "{done:#?}");
        assert_eq!(fs::read_to_string(f.home.join("events")).unwrap(), "pre\n", "the completed hook isn't rerun");
    }
}
