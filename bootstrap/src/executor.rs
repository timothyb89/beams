//! Concurrent execution of a `Task` graph. `graph.rs` only resolves
//! dependencies; this is what actually runs tasks, exploiting all the
//! concurrency the graph allows while respecting
//! `Task::exclusive_resources` for the real-world contention the
//! capability graph doesn't (and shouldn't) know about.
//!
//! Scheduling is dependency-driven, not wave/barrier-based: a task starts
//! the moment every task it `requires` has finished successfully, no
//! matter what else is still running. (An earlier version grouped tasks
//! into dependency "waves" and only started wave N+1 once *all* of wave N
//! was done, so a slow, unrelated task - a big `git clone` - held up
//! everything queued behind it.)
//!
//! Used by `daemon::run_manifest` (templated task instances) and the
//! launch runner (`launch::runner`).

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use serde::Serialize;

use crate::graph;
use crate::tasks::Task;

/// One lifecycle event for a single task's execution. A task that runs
/// gets exactly one `Start`, zero or more `Progress`, then exactly one of
/// `Ok`/`Skip`/`Error`. A task that never runs because something it
/// requires failed gets exactly one `Blocked` and nothing else. Emitted
/// from whichever thread is running that task - `on_event` must tolerate
/// being called concurrently from several threads at once.
pub enum Event {
    Start { task_id: String },
    /// Incremental progress from inside a still-running task's
    /// `run_with_progress` (e.g. a git clone's percent-complete lines).
    /// Not itself a lifecycle state the task passes through, unlike the
    /// other variants.
    Progress { task_id: String, message: String },
    Ok { task_id: String, message: String },
    Skip { task_id: String },
    Error { task_id: String, message: String },
    /// Never started: a task it (transitively) depends on failed.
    Blocked { task_id: String, message: String },
}

/// Named mutual-exclusion locks for `Task::exclusive_resources`, plus one
/// implicit lock per task id. Shareable across several concurrent `run`
/// calls (the daemon keeps one for its whole lifetime - see
/// `daemon::Hub`), so two *separate* manifests that both touch `apt`
/// still serialize against each other, and two submissions of the same
/// task instance (same id, e.g. the same `clone-git-repo` dest) never
/// run at the same time - the second one waits, then typically skips
/// since the first already did the work.
#[derive(Default)]
pub struct Resources {
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Resources {
    /// Runs `f` while holding every lock `task` needs. Locks are acquired
    /// in sorted order, so two tasks declaring overlapping sets can't
    /// deadlock against each other.
    pub fn with_held<R>(&self, task: &dyn Task, f: impl FnOnce() -> R) -> R {
        let mut names: Vec<String> = task
            .exclusive_resources()
            .into_iter()
            .map(|resource| format!("resource:{resource}"))
            .collect();
        names.push(format!("task:{}", task.id()));
        names.sort();
        names.dedup();

        let locks: Vec<Arc<Mutex<()>>> = {
            let mut map = self.locks.lock().unwrap();
            names
                .into_iter()
                .map(|name| map.entry(name).or_insert_with(|| Arc::new(Mutex::new(()))).clone())
                .collect()
        };
        // A poisoned lock just means some earlier holder panicked - the
        // resource itself is still usable, so don't propagate that.
        let _guards: Vec<_> = locks.iter().map(|l| l.lock().unwrap_or_else(|e| e.into_inner())).collect();
        f()
    }
}

/// `run_with` against a fresh, private `Resources` (tests).
#[cfg(test)]
pub fn run(tasks: Vec<Box<dyn Task>>, on_event: &(dyn Fn(Event) + Sync)) -> anyhow::Result<bool> {
    run_with(tasks, &Resources::default(), on_event)
}

/// Resolves `tasks`' dependencies (`graph::resolve_deps`) and runs each
/// one on its own thread as soon as everything it requires has
/// succeeded, holding its `resources` locks for the duration of its run.
///
/// Failure policy: a failed task's dependents (transitively) are never
/// started and are reported as `Blocked`; everything *not* depending on
/// it keeps running and starting normally. In-flight work is never
/// cancelled - an apt install or git clone isn't generally safe to
/// interrupt.
///
/// Returns `Ok(true)` if every task succeeded or skipped, `Ok(false)` if
/// any failed or was blocked, `Err` only if the graph itself doesn't
/// resolve (cycle, missing capability, duplicate provider), before
/// anything has run at all.
#[cfg(test)]
pub fn run_with(
    tasks: Vec<Box<dyn Task>>,
    resources: &Resources,
    on_event: &(dyn Fn(Event) + Sync),
) -> anyhow::Result<bool> {
    run_tagged(tasks, resources, None, on_event)
}

thread_local! {
    static TASK_RUN: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// `<tag>/<task id>` while a task of a tagged run (`run_tagged`) runs on
/// this thread. Tasks put it in their subprocesses' environment as
/// `BEAMS_TASK_RUN`, which the SSH agent relay reports to the workstation
/// (advisory: it's how a launch's SSH key grant tells its own tasks apart
/// from anything else using the key during its window).
pub fn current_task_run() -> Option<String> {
    TASK_RUN.with(|run| run.borrow().clone())
}

/// Runs `f` with `current_task_run()` set to `run`.
pub fn with_task_run<T>(run: Option<String>, f: impl FnOnce() -> T) -> T {
    TASK_RUN.with(|slot| *slot.borrow_mut() = run);
    let out = f();
    TASK_RUN.with(|slot| *slot.borrow_mut() = None);
    out
}

/// `run_with`, tagging each task's thread with `<tag>/<task id>` (see
/// `current_task_run`).
pub fn run_tagged(
    tasks: Vec<Box<dyn Task>>,
    resources: &Resources,
    tag: Option<&str>,
    on_event: &(dyn Fn(Event) + Sync),
) -> anyhow::Result<bool> {
    let deps = graph::resolve_deps(&tasks)?;
    let ids: Vec<String> = tasks.iter().map(|t| t.id()).collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); tasks.len()];
    for (i, d) in deps.iter().enumerate() {
        for &dep in d {
            dependents[dep].push(i);
        }
    }
    let mut unmet: Vec<usize> = deps.iter().map(Vec::len).collect();
    let mut blocked = vec![false; tasks.len()];
    let mut slots: Vec<Option<Box<dyn Task>>> = tasks.into_iter().map(Some).collect();

    let mut overall_ok = true;
    let (done_tx, done_rx) = mpsc::channel::<(usize, bool)>();
    thread::scope(|scope| {
        let mut ready: VecDeque<usize> = (0..slots.len()).filter(|&i| unmet[i] == 0).collect();
        let mut running = 0usize;
        loop {
            while let Some(i) = ready.pop_front() {
                let task = slots[i].take().expect("task scheduled twice");
                let done = DoneSignal { tx: done_tx.clone(), index: i, ok: false };
                running += 1;
                let run = tag.map(|tag| format!("{tag}/{}", task.id()));
                scope.spawn(move || {
                    let mut done = done;
                    done.ok = with_task_run(run, || {
                        resources.with_held(task.as_ref(), || run_one(task.as_ref(), on_event))
                    });
                });
            }
            if running == 0 {
                break;
            }
            let (i, ok) = done_rx.recv().expect("a task thread exited without signalling");
            running -= 1;
            if ok {
                for &d in &dependents[i] {
                    unmet[d] -= 1;
                    if unmet[d] == 0 && !blocked[d] {
                        ready.push_back(d);
                    }
                }
            } else {
                overall_ok = false;
                let mut queue: VecDeque<usize> = dependents[i].iter().copied().collect();
                while let Some(d) = queue.pop_front() {
                    if blocked[d] {
                        continue;
                    }
                    blocked[d] = true;
                    on_event(Event::Blocked {
                        task_id: ids[d].clone(),
                        message: format!("not run: depends on {}, which failed", ids[i]),
                    });
                    queue.extend(dependents[d].iter().copied());
                }
            }
        }
    });
    Ok(overall_ok)
}

#[cfg(test)]
mod task_run_tests {
    use super::*;
    use std::sync::Mutex;

    struct Recorder {
        id: &'static str,
        seen: std::sync::Arc<Mutex<Vec<Option<String>>>>,
    }

    impl Task for Recorder {
        fn id(&self) -> String {
            self.id.to_string()
        }
        fn run(&self) -> anyhow::Result<Option<String>> {
            self.seen.lock().unwrap().push(current_task_run());
            Ok(None)
        }
    }

    #[test]
    fn tagged_runs_expose_tag_and_task_id_to_each_task() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let tasks: Vec<Box<dyn Task>> = vec![
            Box::new(Recorder { id: "a", seen: seen.clone() }),
            Box::new(Recorder { id: "b:dest=/x/y", seen: seen.clone() }),
        ];
        assert!(run_tagged(tasks, &Resources::default(), Some("manifest-7"), &|_| {}).unwrap());
        let mut got = seen.lock().unwrap().clone();
        got.sort();
        assert_eq!(got, vec![Some("manifest-7/a".to_string()), Some("manifest-7/b:dest=/x/y".to_string())]);

        seen.lock().unwrap().clear();
        let tasks: Vec<Box<dyn Task>> = vec![Box::new(Recorder { id: "c", seen: seen.clone() })];
        assert!(run_with(tasks, &Resources::default(), &|_| {}).unwrap());
        assert_eq!(seen.lock().unwrap().clone(), vec![None]);
    }
}

/// Sends this task's completion on drop, so a task thread that panics
/// still unblocks the scheduler loop (as a failure) instead of leaving it
/// waiting forever; the panic itself then propagates when the scope ends.
struct DoneSignal {
    tx: mpsc::Sender<(usize, bool)>,
    index: usize,
    ok: bool,
}

impl Drop for DoneSignal {
    fn drop(&mut self) {
        let _ = self.tx.send((self.index, self.ok));
    }
}

/// Runs one task, emitting its `Start`/`Progress`/terminal events.
/// Returns whether it succeeded (ok or skip). Doesn't take any
/// `Resources` locks itself - see `Resources::with_held`.
pub fn run_one(task: &dyn Task, on_event: &(dyn Fn(Event) + Sync)) -> bool {
    let task_id = task.id();
    on_event(Event::Start { task_id: task_id.clone() });
    let report = |message: &str| {
        on_event(Event::Progress { task_id: task_id.clone(), message: message.to_string() });
    };
    match task.run_with_progress(&report) {
        Ok(Some(message)) => {
            on_event(Event::Ok { task_id, message });
            true
        }
        Ok(None) => {
            on_event(Event::Skip { task_id });
            true
        }
        Err(err) => {
            on_event(Event::Error { task_id, message: err.to_string() });
            false
        }
    }
}

/// One entry of `plan`: a task id plus the ids of the tasks (within the
/// same `run`) it waits on.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct PlannedTask {
    pub task_id: String,
    pub depends_on: Vec<String>,
}

/// What `run` would execute against `tasks`, in a valid topological
/// order, each with its direct dependencies - for a caller that wants to
/// announce the plan (e.g. `daemon::run_manifest`'s `accepted` reply)
/// before anything actually runs.
pub fn plan(tasks: &[Box<dyn Task>]) -> anyhow::Result<Vec<PlannedTask>> {
    let deps = graph::resolve_deps(tasks)?;
    let order = graph::resolve_order(tasks)?;
    Ok(order
        .into_iter()
        .map(|i| PlannedTask {
            task_id: tasks[i].id(),
            depends_on: deps[i].iter().map(|&d| tasks[d].id()).collect(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Capability;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    struct Fake {
        id: &'static str,
        provides: Vec<Capability>,
        requires: Vec<Capability>,
        exclusive: Vec<Capability>,
        delay: Duration,
        fail: bool,
        started: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Task for Fake {
        fn id(&self) -> String {
            self.id.to_string()
        }
        fn provides(&self) -> Vec<Capability> {
            self.provides.clone()
        }
        fn requires(&self) -> Vec<Capability> {
            self.requires.clone()
        }
        fn exclusive_resources(&self) -> Vec<Capability> {
            self.exclusive.clone()
        }
        fn run(&self) -> anyhow::Result<Option<String>> {
            self.started.lock().unwrap().push(self.id);
            thread::sleep(self.delay);
            if self.fail {
                anyhow::bail!("{} failed on purpose", self.id);
            }
            Ok(Some(format!("ran {}", self.id)))
        }
    }

    fn silent_event(_: Event) {}

    #[test]
    fn independent_tasks_run_concurrently_not_serially() {
        // Two tasks with no relationship, each sleeping 200ms: serial
        // execution would take ~400ms+, concurrent should take ~200ms.
        let started = Arc::new(Mutex::new(Vec::new()));
        let make = |id| Box::new(Fake {
            id,
            provides: vec![],
            requires: vec![],
            exclusive: vec![],
            delay: Duration::from_millis(200),
            fail: false,
            started: started.clone(),
        }) as Box<dyn Task>;

        let start = Instant::now();
        let ok = run(vec![make("a"), make("b")], &silent_event).unwrap();
        let elapsed = start.elapsed();

        assert!(ok);
        assert!(
            elapsed < Duration::from_millis(350),
            "expected concurrent execution (~200ms), took {elapsed:?}"
        );
    }

    #[test]
    fn tasks_sharing_an_exclusive_resource_do_not_overlap() {
        let overlap_detected = Arc::new(AtomicUsize::new(0));
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        struct Guarded {
            id: &'static str,
            concurrent: Arc<AtomicUsize>,
            max_concurrent: Arc<AtomicUsize>,
            overlap_detected: Arc<AtomicUsize>,
        }
        impl Task for Guarded {
            fn id(&self) -> String {
                self.id.to_string()
            }
            fn exclusive_resources(&self) -> Vec<Capability> {
                vec!["apt".into()]
            }
            fn run(&self) -> anyhow::Result<Option<String>> {
                let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_concurrent.fetch_max(now, Ordering::SeqCst);
                if now > 1 {
                    self.overlap_detected.fetch_add(1, Ordering::SeqCst);
                }
                thread::sleep(Duration::from_millis(50));
                self.concurrent.fetch_sub(1, Ordering::SeqCst);
                Ok(Some("ok".to_string()))
            }
        }

        let make = |id| {
            Box::new(Guarded {
                id,
                concurrent: concurrent.clone(),
                max_concurrent: max_concurrent.clone(),
                overlap_detected: overlap_detected.clone(),
            }) as Box<dyn Task>
        };

        let ok = run(vec![make("apt-install"), make("install-kubectl")], &silent_event).unwrap();
        assert!(ok);
        assert_eq!(overlap_detected.load(Ordering::SeqCst), 0, "exclusive-resource tasks overlapped");
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }

    fn fake(
        id: &'static str,
        provides: &[&'static str],
        requires: &[&'static str],
        delay_ms: u64,
        fail: bool,
        started: &Arc<Mutex<Vec<&'static str>>>,
    ) -> Box<dyn Task> {
        Box::new(Fake {
            id,
            provides: provides.iter().map(|&s| s.into()).collect(),
            requires: requires.iter().map(|&s| s.into()).collect(),
            exclusive: vec![],
            delay: Duration::from_millis(delay_ms),
            fail,
            started: started.clone(),
        })
    }

    #[test]
    fn a_failure_blocks_only_its_dependents() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let tasks = vec![
            fake("fails", &["x"], &[], 10, true, &started),
            fake("unrelated", &["y"], &[], 50, false, &started),
            fake("after-unrelated", &[], &["y"], 10, false, &started),
            fake("never-runs", &["z"], &["x"], 10, false, &started),
            fake("never-runs-either", &[], &["z"], 10, false, &started),
        ];

        let blocked = Mutex::new(Vec::new());
        let on_event = |event: Event| {
            if let Event::Blocked { task_id, .. } = event {
                blocked.lock().unwrap().push(task_id);
            }
        };
        let ok = run(tasks, &on_event).unwrap();
        assert!(!ok);

        let started = started.lock().unwrap();
        assert!(started.contains(&"fails"));
        assert!(started.contains(&"unrelated"));
        assert!(
            started.contains(&"after-unrelated"),
            "a task whose own dependency succeeded should still run after an unrelated failure"
        );
        assert!(!started.contains(&"never-runs"));
        assert!(!started.contains(&"never-runs-either"));

        let mut blocked = blocked.into_inner().unwrap();
        blocked.sort();
        assert_eq!(blocked, vec!["never-runs", "never-runs-either"]);
    }

    /// The point of dropping wave barriers: "fast-dependent" only needs
    /// "fast", so it shouldn't wait for the unrelated "slow" task that
    /// happens to sit at the same dependency depth as "fast".
    #[test]
    fn a_task_starts_as_soon_as_its_own_dependencies_finish() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let tasks = vec![
            fake("slow", &[], &[], 400, false, &started),
            fake("fast", &["fast"], &[], 10, false, &started),
            fake("fast-dependent", &[], &["fast"], 10, false, &started),
        ];
        let finished = Mutex::new(Vec::new());
        let on_event = |event: Event| {
            if let Event::Ok { task_id, .. } = event {
                finished.lock().unwrap().push(task_id);
            }
        };
        assert!(run(tasks, &on_event).unwrap());
        assert_eq!(finished.into_inner().unwrap(), vec!["fast", "fast-dependent", "slow"]);
    }

    /// Two separate `run_with` calls sharing one `Resources` (what the
    /// daemon does across concurrent manifests) still serialize tasks
    /// with the same id.
    #[test]
    fn shared_resources_serialize_the_same_task_id_across_runs() {
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        struct Same {
            concurrent: Arc<AtomicUsize>,
            max_concurrent: Arc<AtomicUsize>,
        }
        impl Task for Same {
            fn id(&self) -> String {
                "same".to_string()
            }
            fn run(&self) -> anyhow::Result<Option<String>> {
                let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_concurrent.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(50));
                self.concurrent.fetch_sub(1, Ordering::SeqCst);
                Ok(None)
            }
        }
        let resources = Resources::default();
        thread::scope(|scope| {
            for _ in 0..2 {
                let task = Box::new(Same { concurrent: concurrent.clone(), max_concurrent: max_concurrent.clone() });
                let resources = &resources;
                scope.spawn(move || run_with(vec![task], resources, &silent_event).unwrap());
            }
        });
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn plan_lists_direct_dependencies_in_topological_order() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let tasks = vec![
            fake("b", &[], &["a"], 0, false, &started),
            fake("a", &["a"], &[], 0, false, &started),
        ];
        let planned = plan(&tasks).unwrap();
        assert_eq!(
            planned,
            vec![
                PlannedTask { task_id: "a".into(), depends_on: vec![] },
                PlannedTask { task_id: "b".into(), depends_on: vec!["a".into()] },
            ]
        );
    }
}
