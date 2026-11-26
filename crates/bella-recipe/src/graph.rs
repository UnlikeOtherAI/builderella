//! The step graph of one recipe: validation and execution order.

use crate::text::closest;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// One step, as far as the graph is concerned.
pub(crate) struct Node<'a> {
    /// Key path of the step, for messages.
    pub path: &'a str,
    /// `None` when the step has no usable name; it then takes part in the
    /// order but nothing can refer to it.
    pub name: Option<&'a str>,
    pub cleanup: bool,
    /// Names this step needs, each with the key path it was written at.
    pub needs: &'a [(String, String)],
}

pub(crate) struct Problem {
    pub path: String,
    pub message: String,
}

/// Check the graph and return the indices of `nodes` in execution order.
///
/// The order is the topological order that stays closest to the order of
/// declaration: of all steps whose needs are already placed, the one
/// declared first comes next. Cleanup steps are placed after every other
/// step, which is always possible because nothing may need them.
///
/// Every problem is reported: duplicate names, unknown, repeated or
/// self-referring needs, needing a cleanup step, and each dependency cycle.
pub(crate) fn order(nodes: &[Node<'_>]) -> Result<Vec<usize>, Vec<Problem>> {
    let mut problems = Vec::new();

    let mut by_name: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, node) in nodes.iter().enumerate() {
        let Some(name) = node.name else { continue };
        match by_name.entry(name) {
            Entry::Vacant(slot) => {
                slot.insert(index);
            }
            Entry::Occupied(_) => problems.push(Problem {
                path: format!("{}.name", node.path),
                message: format!(
                    "duplicate step name `{name}`: step names must be unique within a recipe"
                ),
            }),
        }
    }
    let names: Vec<&str> = by_name.keys().copied().collect();

    // deps[i] holds the indices step i needs.
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        let mut seen = BTreeSet::new();
        for (need, path) in node.needs {
            let need = need.as_str();
            let message = if !seen.insert(need) {
                Some(format!("`{need}` is listed more than once"))
            } else if node.name == Some(need) {
                Some(format!("step `{need}` cannot need itself"))
            } else {
                match by_name.get(need) {
                    None => {
                        let hint = match closest(need, &names) {
                            Some(name) => format!("; did you mean `{name}`?"),
                            None => String::new(),
                        };
                        Some(format!("no step named `{need}` in this recipe{hint}"))
                    }
                    Some(&target) if nodes[target].cleanup => Some(format!(
                        "`{need}` is a cleanup step; cleanup steps run last, so no step may \
                         need one"
                    )),
                    Some(&target) => {
                        deps[index].push(target);
                        None
                    }
                }
            };
            if let Some(message) = message {
                problems.push(Problem {
                    path: path.clone(),
                    message,
                });
            }
        }
    }

    let mut placed = vec![false; nodes.len()];
    let mut order = Vec::with_capacity(nodes.len());
    for cleanup_pass in [false, true] {
        while let Some(next) = (0..nodes.len()).find(|&i| {
            !placed[i] && nodes[i].cleanup == cleanup_pass && deps[i].iter().all(|&d| placed[d])
        }) {
            placed[next] = true;
            order.push(next);
        }
    }

    // Whatever could not be placed is on a cycle or needs something that is.
    let mut reported = vec![false; nodes.len()];
    for start in 0..nodes.len() {
        if placed[start] || reported[start] {
            continue;
        }
        let Some(cycle) = cycle_through(start, &deps) else {
            continue;
        };
        // Mark the whole strongly connected component, so that one tangle
        // of steps is reported once rather than once per member.
        let reachable = reachable_from(start, &deps);
        for &member in &reachable {
            if reachable_from(member, &deps).contains(&start) {
                reported[member] = true;
            }
        }
        let label = |index: usize| nodes[index].name.unwrap_or("?");
        let mut chain: Vec<&str> = cycle.iter().map(|&index| label(index)).collect();
        chain.push(label(start));
        let entry = cycle.get(1).copied().unwrap_or(start);
        let path = nodes[start]
            .needs
            .iter()
            .find(|(need, _)| need == label(entry))
            .map_or_else(
                || format!("{}.needs", nodes[start].path),
                |(_, path)| path.clone(),
            );
        problems.push(Problem {
            path,
            message: format!("steps need each other in a cycle: {}", chain.join(" -> ")),
        });
    }

    if problems.is_empty() {
        Ok(order)
    } else {
        Err(problems)
    }
}

/// Every step reachable from `start` by following needs, without `start`
/// itself unless a cycle leads back to it.
fn reachable_from(start: usize, deps: &[Vec<usize>]) -> BTreeSet<usize> {
    let mut seen = BTreeSet::new();
    let mut queue: VecDeque<usize> = deps[start].iter().copied().collect();
    while let Some(index) = queue.pop_front() {
        if seen.insert(index) {
            queue.extend(deps[index].iter().copied());
        }
    }
    seen
}

/// The shortest chain `start -> a -> b -> ...` whose last step needs
/// `start` again, if `start` is on a cycle.
fn cycle_through(start: usize, deps: &[Vec<usize>]) -> Option<Vec<usize>> {
    let mut parent: BTreeMap<usize, usize> = BTreeMap::new();
    let mut queue = VecDeque::from([start]);
    while let Some(index) = queue.pop_front() {
        for &next in &deps[index] {
            if next == start {
                let mut chain = vec![index];
                let mut at = index;
                while at != start {
                    at = *parent.get(&at)?;
                    chain.push(at);
                }
                chain.reverse();
                return Some(chain);
            }
            if let Entry::Vacant(slot) = parent.entry(next) {
                slot.insert(index);
                queue.push_back(next);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(name, cleanup, needs)`.
    type Spec<'a> = (&'a str, bool, &'a [&'a str]);

    fn run(specs: &[Spec<'_>]) -> Result<Vec<String>, Vec<String>> {
        let needs: Vec<Vec<(String, String)>> = specs
            .iter()
            .enumerate()
            .map(|(i, (_, _, needs))| {
                needs
                    .iter()
                    .enumerate()
                    .map(|(j, need)| (need.to_string(), format!("steps[{i}].needs[{j}]")))
                    .collect()
            })
            .collect();
        let paths: Vec<String> = (0..specs.len()).map(|i| format!("steps[{i}]")).collect();
        let nodes: Vec<Node<'_>> = specs
            .iter()
            .enumerate()
            .map(|(i, (name, cleanup, _))| Node {
                path: &paths[i],
                name: Some(*name),
                cleanup: *cleanup,
                needs: &needs[i],
            })
            .collect();
        match order(&nodes) {
            Ok(order) => Ok(order.iter().map(|&i| specs[i].0.to_string()).collect()),
            Err(problems) => Err(problems
                .into_iter()
                .map(|p| format!("{}: {}", p.path, p.message))
                .collect()),
        }
    }

    #[test]
    fn independent_steps_keep_declaration_order() {
        let order = run(&[("c", false, &[]), ("a", false, &[]), ("b", false, &[])]).unwrap();
        assert_eq!(order, ["c", "a", "b"]);
    }

    #[test]
    fn a_step_moves_only_as_far_as_its_needs_require() {
        // `test` is declared first but needs `build` and `db`; `lint` has
        // no needs and keeps its place relative to the others.
        let order = run(&[
            ("test", false, &["build", "db"]),
            ("lint", false, &[]),
            ("db", false, &[]),
            ("build", false, &["gen"]),
            ("gen", false, &[]),
        ])
        .unwrap();
        assert_eq!(order, ["lint", "db", "gen", "build", "test"]);
    }

    #[test]
    fn the_order_is_deterministic() {
        let specs: [Spec<'_>; 5] = [
            ("e", false, &["a"]),
            ("d", false, &["a"]),
            ("a", false, &[]),
            ("c", false, &["d", "e"]),
            ("b", false, &[]),
        ];
        let first = run(&specs).unwrap();
        assert_eq!(first, ["a", "e", "d", "c", "b"]);
        for _ in 0..5 {
            assert_eq!(run(&specs).unwrap(), first);
        }
    }

    #[test]
    fn cleanup_steps_come_last() {
        let order = run(&[
            ("wipe", true, &[]),
            ("build", false, &[]),
            ("report", true, &["build"]),
            ("test", false, &["build"]),
        ])
        .unwrap();
        assert_eq!(order, ["build", "test", "wipe", "report"]);
    }

    #[test]
    fn nothing_may_need_a_cleanup_step() {
        let problems = run(&[("wipe", true, &[]), ("test", false, &["wipe"])]).unwrap_err();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].starts_with("steps[1].needs[0]:"),
            "{problems:?}"
        );
        assert!(problems[0].contains("cleanup step"), "{problems:?}");

        // Not even another cleanup step.
        let problems = run(&[("wipe", true, &[]), ("after", true, &["wipe"])]).unwrap_err();
        assert!(problems[0].contains("cleanup step"), "{problems:?}");
    }

    #[test]
    fn bad_references_are_all_reported() {
        let problems = run(&[
            ("build", false, &["biuld", "build", "nope"]),
            ("test", false, &["build", "build"]),
            ("build", false, &[]),
        ])
        .unwrap_err();
        let text = problems.join("\n");
        assert_eq!(problems.len(), 5, "{text}");
        assert!(
            text.contains(
                "steps[0].needs[0]: no step named `biuld` in this recipe; did you mean `build`?"
            ),
            "{text}"
        );
        assert!(
            text.contains("steps[0].needs[1]: step `build` cannot need itself"),
            "{text}"
        );
        assert!(
            text.contains("steps[0].needs[2]: no step named `nope`"),
            "{text}"
        );
        assert!(
            text.contains("steps[1].needs[1]: `build` is listed more than once"),
            "{text}"
        );
        assert!(
            text.contains("steps[2].name: duplicate step name `build`"),
            "{text}"
        );
    }

    #[test]
    fn a_cycle_is_reported_once_with_its_chain() {
        let problems = run(&[
            ("a", false, &["c"]),
            ("b", false, &["a"]),
            ("c", false, &["b"]),
            ("after", false, &["a"]),
        ])
        .unwrap_err();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(
            problems[0],
            "steps[0].needs[0]: steps need each other in a cycle: a -> c -> b -> a"
        );
    }

    #[test]
    fn separate_cycles_are_each_reported() {
        let problems = run(&[
            ("a", false, &["b"]),
            ("b", false, &["a"]),
            ("ok", false, &[]),
            ("x", false, &["a", "y"]),
            ("y", false, &["x"]),
        ])
        .unwrap_err();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].ends_with("a -> b -> a"), "{problems:?}");
        assert!(
            problems[1].starts_with("steps[3].needs[1]:"),
            "{problems:?}"
        );
        assert!(problems[1].ends_with("x -> y -> x"), "{problems:?}");
    }

    #[test]
    fn an_empty_recipe_has_an_empty_order() {
        assert_eq!(run(&[]), Ok(Vec::new()));
    }
}
