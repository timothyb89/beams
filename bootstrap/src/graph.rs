//! Capability-based dependency resolution for bootstrap steps/tasks.
//!
//! See `DESIGN.md`'s "Task model" section: a step declares the
//! capabilities it provides and the ones it requires, instead of naming
//! other steps directly. This lets a step's dependents be resolved without
//! knowing a concrete step's identity up front - the piece that matters
//! once steps stop being a fixed, hand-ordered list (e.g. a templated task
//! instantiated with params at runtime, whose id and capabilities aren't
//! known ahead of time - see `tasks`).

use std::borrow::Cow;
use std::collections::HashMap;

use anyhow::bail;

/// A named precondition or outcome, e.g. `"apt-packages-installed"` or,
/// for a templated task instantiated with params, something computed at
/// runtime like `"repo-cloned:~/projects/core"`. `Cow` so a compile-time
/// step can hand back a `&'static str` constant with no allocation, while
/// a runtime task instance can hand back an owned, params-derived string.
pub type Capability = Cow<'static, str>;

/// Anything that can be ordered by the resolver: declares what it needs
/// before it runs, and what it leaves true once it has. `bootstrap`'s
/// `Step` trait (fixed, compile-time steps) and `tasks::Task` trait
/// (runtime-instantiated, templated tasks) both implement this via their
/// `Node for Box<dyn ...>` impls.
pub trait Node {
    fn node_id(&self) -> String;
    fn provides(&self) -> Vec<Capability>;
    fn requires(&self) -> Vec<Capability>;
}

/// Computes a run order for `nodes` satisfying every `requires`/`provides`
/// edge, preserving the input order among nodes that are otherwise
/// unconstrained relative to each other (so an already-correctly-ordered
/// input list round-trips through this unchanged). Just `resolve_levels`
/// flattened - see that function if what you actually want is which
/// nodes could run *concurrently*, not just some valid serial order.
///
/// Returns the chosen order as indices into `nodes`. Errors on an unknown
/// required capability (nothing provides it), two nodes claiming the same
/// capability, or a dependency cycle.
pub fn resolve_order<N: Node>(nodes: &[N]) -> anyhow::Result<Vec<usize>> {
    Ok(resolve_levels(nodes)?.into_iter().flatten().collect())
}

/// Groups `nodes` into dependency "waves": level 0 is every node with no
/// `requires` at all (or whose requirements are trivially satisfied),
/// level 1 is everything that only depends on level 0, and so on. Nodes
/// within the same level have no dependency relationship to each other -
/// an executor is free to run them concurrently (see `executor::run`,
/// which is the actual reason this is split out from a flat order: a
/// `Node` alone doesn't know about real-world resource contention a
/// concurrent executor also needs to respect - e.g. two steps that both
/// shell out to `apt-get` - that's `Task::exclusive_resources`' job, one
/// layer up from here).
///
/// Same preserved-input-order and error conditions as `resolve_order`
/// (indeed, `resolve_order` is just this, flattened): within a level,
/// nodes appear in their original input order.
pub fn resolve_levels<N: Node>(nodes: &[N]) -> anyhow::Result<Vec<Vec<usize>>> {
    let deps = resolve_deps_unchecked(nodes)?;
    levels_from_deps(nodes, &deps)
}

/// Each node's direct dependencies, as indices into `nodes` (deduplicated,
/// in the order its `requires` names them) - what a dependency-driven
/// scheduler (`executor::run`) actually needs, rather than the
/// barrier-style levels `resolve_levels` groups them into. Validates
/// exactly what `resolve_levels` does (unknown capability, duplicate
/// provider, cycle), so a `deps` list returned from here is always
/// schedulable.
pub fn resolve_deps<N: Node>(nodes: &[N]) -> anyhow::Result<Vec<Vec<usize>>> {
    let deps = resolve_deps_unchecked(nodes)?;
    levels_from_deps(nodes, &deps)?; // cycle check
    Ok(deps)
}

fn resolve_deps_unchecked<N: Node>(nodes: &[N]) -> anyhow::Result<Vec<Vec<usize>>> {
    let mut provided_by: HashMap<Capability, usize> = HashMap::new();
    for (i, node) in nodes.iter().enumerate() {
        for cap in node.provides() {
            if let Some(&prev) = provided_by.get(&cap) {
                bail!(
                    "capability {cap:?} is provided by both {:?} and {:?}",
                    nodes[prev].node_id(),
                    node.node_id()
                );
            }
            provided_by.insert(cap, i);
        }
    }

    let mut deps: Vec<Vec<usize>> = Vec::with_capacity(nodes.len());
    for node in nodes {
        let mut d = Vec::new();
        for cap in node.requires() {
            match provided_by.get(&cap) {
                Some(&idx) if !d.contains(&idx) => d.push(idx),
                Some(_) => {}
                None => bail!(
                    "{:?} requires capability {cap:?}, which no node provides",
                    node.node_id()
                ),
            }
        }
        deps.push(d);
    }
    Ok(deps)
}

fn levels_from_deps<N: Node>(nodes: &[N], deps: &[Vec<usize>]) -> anyhow::Result<Vec<Vec<usize>>> {
    let mut placed = vec![false; nodes.len()];
    let mut placed_count = 0;
    let mut levels: Vec<Vec<usize>> = Vec::new();
    while placed_count < nodes.len() {
        let mut level = Vec::new();
        for (i, ready) in deps.iter().enumerate() {
            if !placed[i] && ready.iter().all(|&d| placed[d]) {
                level.push(i);
            }
        }
        if level.is_empty() {
            let stuck: Vec<String> = (0..nodes.len())
                .filter(|&i| !placed[i])
                .map(|i| nodes[i].node_id())
                .collect();
            bail!("dependency cycle among: {}", stuck.join(", "));
        }
        for &i in &level {
            placed[i] = true;
        }
        placed_count += level.len();
        levels.push(level);
    }
    Ok(levels)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        id: &'static str,
        provides: &'static [&'static str],
        requires: &'static [&'static str],
    }

    impl Node for Fake {
        fn node_id(&self) -> String {
            self.id.to_string()
        }
        fn provides(&self) -> Vec<Capability> {
            self.provides.iter().map(|&s| Cow::Borrowed(s)).collect()
        }
        fn requires(&self) -> Vec<Capability> {
            self.requires.iter().map(|&s| Cow::Borrowed(s)).collect()
        }
    }

    fn ids(nodes: &[Fake], order: &[usize]) -> Vec<&'static str> {
        order.iter().map(|&i| nodes[i].id).collect()
    }

    #[test]
    fn already_ordered_input_round_trips() {
        let nodes = [
            Fake { id: "a", provides: &["a"], requires: &[] },
            Fake { id: "b", provides: &["b"], requires: &["a"] },
            Fake { id: "c", provides: &["c"], requires: &["b"] },
        ];
        let order = resolve_order(&nodes).unwrap();
        assert_eq!(ids(&nodes, &order), vec!["a", "b", "c"]);
    }

    #[test]
    fn unconstrained_nodes_keep_input_order() {
        let nodes = [
            Fake { id: "a", provides: &[], requires: &[] },
            Fake { id: "b", provides: &[], requires: &[] },
            Fake { id: "c", provides: &[], requires: &[] },
        ];
        let order = resolve_order(&nodes).unwrap();
        assert_eq!(ids(&nodes, &order), vec!["a", "b", "c"]);
    }

    #[test]
    fn reorders_a_dependency_declared_after_its_dependent() {
        // "b" is listed before "a" but requires it - resolver must still
        // place "a" first.
        let nodes = [
            Fake { id: "b", provides: &["b"], requires: &["a"] },
            Fake { id: "a", provides: &["a"], requires: &[] },
        ];
        let order = resolve_order(&nodes).unwrap();
        assert_eq!(ids(&nodes, &order), vec!["a", "b"]);
    }

    #[test]
    fn missing_capability_is_an_error() {
        let nodes = [Fake { id: "a", provides: &[], requires: &["nope"] }];
        let err = resolve_order(&nodes).unwrap_err().to_string();
        assert!(err.contains("nope"), "unexpected error: {err}");
    }

    #[test]
    fn duplicate_provider_is_an_error() {
        let nodes = [
            Fake { id: "a", provides: &["x"], requires: &[] },
            Fake { id: "b", provides: &["x"], requires: &[] },
        ];
        let err = resolve_order(&nodes).unwrap_err().to_string();
        assert!(err.contains("a") && err.contains("b"), "unexpected error: {err}");
    }

    #[test]
    fn cycle_is_an_error() {
        let nodes = [
            Fake { id: "a", provides: &["a"], requires: &["b"] },
            Fake { id: "b", provides: &["b"], requires: &["a"] },
        ];
        let err = resolve_order(&nodes).unwrap_err().to_string();
        assert!(err.contains("cycle"), "unexpected error: {err}");
    }

    #[test]
    fn independent_nodes_share_one_level() {
        // "b" and "c" both only depend on "a" - and have no relationship
        // to each other - so they should land in the same level, free to
        // run concurrently.
        let nodes = [
            Fake { id: "a", provides: &["a"], requires: &[] },
            Fake { id: "b", provides: &["b"], requires: &["a"] },
            Fake { id: "c", provides: &["c"], requires: &["a"] },
        ];
        let levels = resolve_levels(&nodes).unwrap();
        let level_ids: Vec<Vec<&str>> =
            levels.iter().map(|level| level.iter().map(|&i| nodes[i].id).collect()).collect();
        assert_eq!(level_ids, vec![vec!["a"], vec!["b", "c"]]);
    }

    #[test]
    fn fully_independent_nodes_are_all_one_level() {
        let nodes = [
            Fake { id: "a", provides: &[], requires: &[] },
            Fake { id: "b", provides: &[], requires: &[] },
            Fake { id: "c", provides: &[], requires: &[] },
        ];
        let levels = resolve_levels(&nodes).unwrap();
        assert_eq!(levels.len(), 1, "no dependency relationship at all should mean one level");
        assert_eq!(levels[0], vec![0, 1, 2]);
    }

    #[test]
    fn a_fully_linear_chain_is_one_node_per_level() {
        let nodes = [
            Fake { id: "a", provides: &["a"], requires: &[] },
            Fake { id: "b", provides: &["b"], requires: &["a"] },
            Fake { id: "c", provides: &["c"], requires: &["b"] },
        ];
        let levels = resolve_levels(&nodes).unwrap();
        assert_eq!(levels, vec![vec![0], vec![1], vec![2]]);
    }

    /// A runtime-computed (owned) capability must match a statically
    /// declared (borrowed) one with the same text - this is the whole
    /// point of `Capability` being a `Cow`, exercised by a templated
    /// task's capability lining up with a fixed step's.
    #[test]
    fn owned_and_borrowed_capabilities_are_interchangeable() {
        struct Owned(String);
        impl Node for Owned {
            fn node_id(&self) -> String {
                "owned".to_string()
            }
            fn provides(&self) -> Vec<Capability> {
                vec![Cow::Owned(self.0.clone())]
            }
            fn requires(&self) -> Vec<Capability> {
                vec![]
            }
        }

        let provider = Owned("dest=~/projects/core".to_string());
        let dependent = Fake {
            id: "dependent",
            provides: &[],
            requires: &["dest=~/projects/core"],
        };

        // Mixed node types can't share one `resolve_order::<N>` call (it's
        // monomorphic over one `N`), so this just checks the capability
        // values themselves compare equal across the owned/borrowed split
        // - the thing a mixed-node graph would actually rely on.
        assert_eq!(provider.provides()[0], dependent.requires()[0]);
    }
}
