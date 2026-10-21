//! Capability-based dependency resolution for bootstrap steps/tasks.
//!
//! See `DESIGN.md`'s "Task model" section: a step declares the
//! capabilities it provides and the ones it requires, instead of naming
//! other steps directly. This lets a step's dependents be resolved without
//! knowing a concrete step's identity up front - the piece that matters
//! once steps stop being a fixed, hand-ordered list (e.g. a templated task
//! instantiated with params at runtime, whose id isn't known ahead of
//! time, but whose *capability* is).

use std::collections::HashMap;

use anyhow::bail;

/// A named precondition or outcome. Just a string today (e.g.
/// `"apt-packages-installed"`) - no need for anything richer yet.
pub type Capability = &'static str;

/// Anything that can be ordered by the resolver: declares what it needs
/// before it runs, and what it leaves true once it has. `bootstrap`'s
/// `Step` trait implements this; a future daemon task would too.
pub trait Node {
    fn node_id(&self) -> &str;
    fn provides(&self) -> &'static [Capability];
    fn requires(&self) -> &'static [Capability];
}

/// Computes a run order for `nodes` satisfying every `requires`/`provides`
/// edge, preserving the input order among nodes that are otherwise
/// unconstrained relative to each other (so an already-correctly-ordered
/// input list round-trips through this unchanged - important while this
/// is introduced alongside the existing hand-ordered step list, which
/// should see no behavior change).
///
/// Returns the chosen order as indices into `nodes`. Errors on an unknown
/// required capability (nothing provides it), two nodes claiming the same
/// capability, or a dependency cycle.
pub fn resolve_order<N: Node>(nodes: &[N]) -> anyhow::Result<Vec<usize>> {
    let mut provided_by: HashMap<Capability, usize> = HashMap::new();
    for (i, node) in nodes.iter().enumerate() {
        for cap in node.provides() {
            if let Some(&prev) = provided_by.get(cap) {
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
            match provided_by.get(cap) {
                Some(&idx) => d.push(idx),
                None => bail!(
                    "{:?} requires capability {cap:?}, which no node provides",
                    node.node_id()
                ),
            }
        }
        deps.push(d);
    }

    let mut placed = vec![false; nodes.len()];
    let mut order = Vec::with_capacity(nodes.len());
    while order.len() < nodes.len() {
        let mut progressed = false;
        for (i, ready) in deps.iter().enumerate() {
            if !placed[i] && ready.iter().all(|&d| placed[d]) {
                placed[i] = true;
                order.push(i);
                progressed = true;
            }
        }
        if !progressed {
            let stuck: Vec<&str> = (0..nodes.len())
                .filter(|&i| !placed[i])
                .map(|i| nodes[i].node_id())
                .collect();
            bail!("dependency cycle among: {}", stuck.join(", "));
        }
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        id: &'static str,
        provides: &'static [Capability],
        requires: &'static [Capability],
    }

    impl Node for Fake {
        fn node_id(&self) -> &str {
            self.id
        }
        fn provides(&self) -> &'static [Capability] {
            self.provides
        }
        fn requires(&self) -> &'static [Capability] {
            self.requires
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
}
