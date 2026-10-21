//! Concurrent execution of a `Task` graph. `graph.rs` only resolves
//! *order* (and now, waves); this is what actually runs tasks, exploiting
//! the concurrency the graph allows while respecting
//! `Task::exclusive_resources` for the real-world contention the
//! capability graph doesn't (and shouldn't) know about.
//!
//! Used by both `daemon::run_manifest` (templated task instances) and
//! `steps::run` (the fixed startup sequence, via `steps::StepAsTask` -
//! the adapter that turns a `Step` into a `Task` so the two can share one
//! graph/executor instead of two separate, incompatible run loops).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::graph::{self, Capability};
use crate::tasks::Task;

/// One lifecycle event for a single task's execution, in the order a
/// caller should expect them: exactly one `Start`, then exactly one of
/// `Ok`/`Skip`/`Error`. Emitted from whichever thread is running that
/// task - `on_event` must tolerate being called concurrently from
/// several threads at once (tasks in the same wave run in parallel).
pub enum Event {
    Start { task_id: String },
    Ok { task_id: String, message: String },
    Skip { task_id: String },
    Error { task_id: String, message: String },
}

/// Resolves `tasks` into dependency waves (`graph::resolve_levels`) and
/// runs them wave by wave: every task in a wave starts on its own
/// thread, serialized against other tasks in the *same* wave only when
/// they share an `exclusive_resources` entry (locks acquired in sorted
/// order, so a task declaring more than one resource can't deadlock
/// against another task declaring the same set). The next wave only
/// starts once every task in the current one has finished.
///
/// Stops after a wave containing any failure - no later wave starts. A
/// wave already in flight when one of its members fails runs every
/// other member to completion rather than trying to cancel real,
/// possibly-destructive work (an apt install, a git clone) mid-flight.
///
/// Returns `Ok(true)` if every task succeeded or skipped, `Ok(false)` if
/// some task failed (after running everything that was safe to run),
/// `Err` only if the graph itself doesn't resolve (cycle, missing
/// capability, duplicate provider - see `graph::resolve_levels`), before
/// anything has run at all.
pub fn run(tasks: Vec<Box<dyn Task>>, on_event: &(dyn Fn(Event) + Sync)) -> anyhow::Result<bool> {
    let levels = graph::resolve_levels(&tasks)?;
    let mut slots: Vec<Option<Box<dyn Task>>> = tasks.into_iter().map(Some).collect();
    let resource_locks: Mutex<HashMap<Capability, Arc<Mutex<()>>>> = Mutex::new(HashMap::new());

    let mut overall_ok = true;
    for level in levels {
        let level_ok: Mutex<bool> = Mutex::new(true);
        thread::scope(|scope| {
            for &i in &level {
                let task = slots[i].take().expect("resolve_levels repeats an index");

                let mut resources = task.exclusive_resources();
                resources.sort();
                let locks: Vec<Arc<Mutex<()>>> = resources
                    .into_iter()
                    .map(|resource| {
                        resource_locks
                            .lock()
                            .unwrap()
                            .entry(resource)
                            .or_insert_with(|| Arc::new(Mutex::new(())))
                            .clone()
                    })
                    .collect();

                let level_ok = &level_ok;
                scope.spawn(move || {
                    // Held for exactly this task's run() - two tasks
                    // sharing a resource serialize against each other
                    // here, without blocking anything else in the wave.
                    let _guards: Vec<_> = locks.iter().map(|l| l.lock().unwrap()).collect();

                    let task_id = task.id();
                    on_event(Event::Start { task_id: task_id.clone() });
                    match task.run() {
                        Ok(Some(message)) => on_event(Event::Ok { task_id, message }),
                        Ok(None) => on_event(Event::Skip { task_id }),
                        Err(err) => {
                            on_event(Event::Error { task_id, message: err.to_string() });
                            *level_ok.lock().unwrap() = false;
                        }
                    }
                });
            }
        });

        if !*level_ok.lock().unwrap() {
            overall_ok = false;
            break;
        }
    }
    Ok(overall_ok)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn a_failure_stops_later_waves_but_not_the_rest_of_its_own_wave() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let tasks: Vec<Box<dyn Task>> = vec![
            Box::new(Fake {
                id: "fails",
                provides: vec!["x".into()],
                requires: vec![],
                exclusive: vec![],
                delay: Duration::from_millis(10),
                fail: true,
                started: started.clone(),
            }),
            Box::new(Fake {
                id: "unrelated-same-wave",
                provides: vec![],
                requires: vec![],
                exclusive: vec![],
                delay: Duration::from_millis(50),
                fail: false,
                started: started.clone(),
            }),
            Box::new(Fake {
                id: "never-runs",
                provides: vec![],
                requires: vec!["x".into()],
                exclusive: vec![],
                delay: Duration::from_millis(10),
                fail: false,
                started: started.clone(),
            }),
        ];

        let ok = run(tasks, &silent_event).unwrap();
        assert!(!ok);

        let started = started.lock().unwrap();
        assert!(started.contains(&"fails"));
        assert!(
            started.contains(&"unrelated-same-wave"),
            "a task in the same wave as a failure, with no dependency on it, should still run to completion"
        );
        assert!(
            !started.contains(&"never-runs"),
            "a task depending on a failed one should never start"
        );
    }
}
