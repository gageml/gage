//! The plan: one DAG over every task of the scan.
//!
//! Tasks are the nodes. An edge runs from an upstream task to a
//! downstream task when a `wants` or `required_by` pattern of the
//! downstream task matches a name under the upstream task's `writes`.
//! Patterns are `*`-globs matched across every planned scanner, so a
//! task in one scanner is ordered after a task in another without
//! naming it. An edge is ordering only: the downstream task runs
//! after its upstream tasks finish, whatever their status, and reads
//! what exists. Dependencies resolve over notes only.
//!
//! A cycle is a plan error, since the edges are inferred and the
//! person cannot see them. A `wants` pattern no planned task writes
//! is recorded on the task as unmatched and reported as a warning.
//! Task order is scanner name then task name, which is the dispatch
//! tie-break order.
//!
//! The plan is recorded as `scan/plan.json` (see [`Plan::to_json`]),
//! written at create so it exists for a scan canceled before its
//! first task.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use gage_core::glob::glob_match;
use gage_registry::scanner::TaskDef;
use serde::Serialize;

/// One scanner's contribution to the plan: its planned tasks and how
/// it was selected.
pub struct PlannedScanner<'a> {
    pub name: &'a str,
    pub tasks: &'a BTreeMap<String, TaskDef>,
    pub selection: Selection,
}

/// How a scanner entered the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Named on the command line or given as a file
    Explicit,
    /// A member of a selected group
    Group(String),
    /// Pulled in because a task's `required_by` matched a planned
    /// write; the plan records the matching pattern per task
    RequiredBy,
}

/// How a task entered the plan, as recorded in `plan.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selected {
    Explicit,
    Group(String),
    RequiredBy(String),
}

impl fmt::Display for Selected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Selected::Explicit => write!(f, "explicit"),
            Selected::Group(name) => write!(f, "group:{name}"),
            Selected::RequiredBy(pattern) => write!(f, "required_by:{pattern}"),
        }
    }
}

/// An ordering edge into a task from one upstream task, with the
/// pattern that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub upstream: usize,
    pub pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTask {
    pub scanner: String,
    pub task: String,
    pub selected: Selected,
    /// Every edge into this task, in upstream order
    pub after: Vec<Edge>,
    /// `wants` patterns no planned task writes
    pub unmatched: Vec<String>,
    /// The note names the task declares it writes, each with its
    /// doc. The plan records them so a scan carries the docs of the
    /// notes it holds; the `note_doc` table of the scan's scope reads
    /// them from here.
    pub writes: BTreeMap<String, String>,
}

impl PlanTask {
    /// `<scanner>:<task>`
    pub fn label(&self) -> String {
        format!("{}:{}", self.scanner, self.task)
    }
}

pub struct Plan {
    /// Tasks in dispatch tie-break order
    pub tasks: Vec<PlanTask>,
    /// `downstream[i]` are the tasks with an edge from task `i`
    pub downstream: Vec<Vec<usize>>,
    /// The in-degree of each task
    pub deps: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The tasks on a dependency cycle, as `<scanner>:<task>`
    Cycle(Vec<String>),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::Cycle(tasks) => {
                write!(f, "cycle in task dependencies: {}", tasks.join(" -> "))
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// Build the plan over every task of `scanners`. Scanner names must be
/// unique.
#[expect(
    clippy::indexing_slicing,
    reason = "task indices are plan-internal and bounded by construction"
)]
pub fn plan(scanners: &[PlannedScanner<'_>]) -> Result<Plan, PlanError> {
    // Tasks in (scanner, task) order; `tasks` in a BTreeMap is already
    // in task order
    let mut order: Vec<&PlannedScanner<'_>> = scanners.iter().collect();
    order.sort_by(|a, b| a.name.cmp(b.name));
    let mut defs: Vec<(&PlannedScanner<'_>, &TaskDef)> = Vec::new();
    for scanner in order {
        for def in scanner.tasks.values() {
            defs.push((scanner, def));
        }
    }

    // Written note name -> tasks writing it, across every scanner
    let mut writers: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, (_, def)) in defs.iter().enumerate() {
        for name in def.notes.writes.keys() {
            writers.entry(name.as_str()).or_default().push(i);
        }
    }

    // The tasks writing a name matching `pattern`, excluding `this`
    // so a task wanting what it writes gets no self edge
    let matching = |pattern: &str, this: usize| -> (bool, Vec<usize>) {
        let mut matched = false;
        let mut out = Vec::new();
        for (name, tasks) in &writers {
            if !glob_match(pattern, name) {
                continue;
            }
            matched = true;
            out.extend(tasks.iter().copied().filter(|t| *t != this));
        }
        out.sort_unstable();
        out.dedup();
        (matched, out)
    };

    let mut tasks: Vec<PlanTask> = Vec::with_capacity(defs.len());
    let mut edges: HashSet<(usize, usize)> = HashSet::new();
    for (i, (scanner, def)) in defs.iter().enumerate() {
        let mut after = Vec::new();
        let mut unmatched = Vec::new();
        for pattern in &def.notes.wants {
            let (matched, upstream) = matching(pattern, i);
            if !matched {
                unmatched.push(pattern.clone());
            }
            for up in upstream {
                after.push(Edge {
                    upstream: up,
                    pattern: pattern.clone(),
                });
                edges.insert((up, i));
            }
        }
        // A pulled-in task is ordered after what pulled it in, as
        // `wants` would order it, without the unmatched warning
        for pattern in &def.notes.required_by {
            let (_, upstream) = matching(pattern, i);
            for up in upstream {
                after.push(Edge {
                    upstream: up,
                    pattern: pattern.clone(),
                });
                edges.insert((up, i));
            }
        }
        after.sort_by(|a, b| {
            a.upstream
                .cmp(&b.upstream)
                .then_with(|| a.pattern.cmp(&b.pattern))
        });
        after.dedup();
        let selected = match &scanner.selection {
            Selection::Explicit => Selected::Explicit,
            Selection::Group(name) => Selected::Group(name.clone()),
            Selection::RequiredBy => {
                let pattern = def
                    .notes
                    .required_by
                    .iter()
                    .find(|p| matching(p, i).0)
                    .cloned()
                    .unwrap_or_default();
                Selected::RequiredBy(pattern)
            }
        };
        tasks.push(PlanTask {
            scanner: scanner.name.to_string(),
            task: def.name.clone(),
            selected,
            after,
            unmatched,
            writes: def.notes.writes.clone(),
        });
    }

    let mut downstream = vec![Vec::new(); tasks.len()];
    let mut deps = vec![0u32; tasks.len()];
    for (up, down) in &edges {
        downstream[*up].push(*down);
        deps[*down] += 1;
    }
    for d in &mut downstream {
        d.sort_unstable();
    }

    if let Some(cycle) = find_cycle(&downstream, &deps) {
        return Err(PlanError::Cycle(
            cycle.into_iter().map(|i| tasks[i].label()).collect(),
        ));
    }

    Ok(Plan {
        tasks,
        downstream,
        deps,
    })
}

/// The tasks on a cycle, in index order, or `None` when the graph is
/// acyclic. Peels nodes with no remaining in-edges, then nodes with no
/// remaining out-edges; what is left lies on a cycle.
#[expect(
    clippy::indexing_slicing,
    reason = "task indices are plan-internal and bounded by construction"
)]
fn find_cycle(downstream: &[Vec<usize>], deps: &[u32]) -> Option<Vec<usize>> {
    let n = deps.len();
    let mut remaining: Vec<bool> = vec![true; n];
    let mut in_degree: Vec<u32> = deps.to_vec();
    let mut queue: Vec<usize> = (0..n).filter(|i| in_degree[*i] == 0).collect();
    while let Some(i) = queue.pop() {
        remaining[i] = false;
        for &d in &downstream[i] {
            in_degree[d] -= 1;
            if in_degree[d] == 0 {
                queue.push(d);
            }
        }
    }
    if remaining.iter().all(|r| !r) {
        return None;
    }
    let mut out_degree: Vec<usize> = (0..n)
        .map(|i| downstream[i].iter().filter(|d| remaining[**d]).count())
        .collect();
    let mut upstream: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, downs) in downstream.iter().enumerate() {
        for &d in downs {
            upstream.entry(d).or_default().push(i);
        }
    }
    let mut queue: Vec<usize> = (0..n)
        .filter(|i| remaining[*i] && out_degree[*i] == 0)
        .collect();
    while let Some(i) = queue.pop() {
        remaining[i] = false;
        for &u in upstream.get(&i).map(Vec::as_slice).unwrap_or(&[]) {
            if !remaining[u] {
                continue;
            }
            out_degree[u] -= 1;
            if out_degree[u] == 0 {
                queue.push(u);
            }
        }
    }
    Some((0..n).filter(|i| remaining[*i]).collect())
}

impl Plan {
    /// The `plan.json` content: per task, how it entered the plan and
    /// what it was ordered after, in dispatch tie-break order.
    #[expect(
        clippy::indexing_slicing,
        reason = "task indices are plan-internal and bounded by construction"
    )]
    pub fn to_json(&self) -> serde_json::Value {
        let tasks: Vec<TaskJson> = self
            .tasks
            .iter()
            .map(|t| TaskJson {
                task: t.label(),
                selected: t.selected.to_string(),
                after: t
                    .after
                    .iter()
                    .map(|e| EdgeJson {
                        task: self.tasks[e.upstream].label(),
                        pattern: e.pattern.clone(),
                    })
                    .collect(),
                unmatched: t.unmatched.clone(),
                writes: t.writes.clone(),
            })
            .collect();
        serde_json::to_value(PlanJson { tasks }).expect("plan fields are plain data")
    }
}

#[derive(Serialize)]
struct PlanJson {
    tasks: Vec<TaskJson>,
}

#[derive(Serialize)]
struct TaskJson {
    task: String,
    selected: String,
    after: Vec<EdgeJson>,
    unmatched: Vec<String>,
    writes: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct EdgeJson {
    task: String,
    pattern: String,
}

#[cfg(test)]
mod tests {
    use gage_registry::scanner::TaskDepsDef;

    use super::*;

    /// `(task, wants, writes, required_by)`
    type Entry<'a> = (&'a str, &'a [&'a str], &'a [&'a str], &'a [&'a str]);

    fn tasks(entries: &[Entry<'_>]) -> BTreeMap<String, TaskDef> {
        entries
            .iter()
            .map(|(name, wants, writes, required_by)| {
                let strings = |list: &[&str]| list.iter().map(|s| s.to_string()).collect();
                (
                    name.to_string(),
                    TaskDef {
                        name: name.to_string(),
                        notes: TaskDepsDef {
                            wants: strings(wants),
                            writes: writes
                                .iter()
                                .map(|w| (w.to_string(), String::new()))
                                .collect(),
                            required_by: strings(required_by),
                        },
                        issues: TaskDepsDef::default(),
                    },
                )
            })
            .collect()
    }

    fn idx(plan: &Plan, label: &str) -> usize {
        plan.tasks
            .iter()
            .position(|t| t.label() == label)
            .unwrap_or_else(|| panic!("no task {label} in plan"))
    }

    fn assert_edge(plan: &Plan, up: &str, down: &str) {
        let (up, down) = (idx(plan, up), idx(plan, down));
        assert!(
            plan.downstream[up].contains(&down),
            "expected edge {up} -> {down}: {:?}",
            plan.downstream
        );
        assert!(plan.deps[down] > 0);
    }

    #[test]
    fn wants_orders_the_downstream_task_across_scanners() {
        let a = tasks(&[("write", &[], &["finding"], &[])]);
        let b = tasks(&[("read", &["finding"], &[], &[])]);
        let plan = plan(&[
            PlannedScanner {
                name: "b",
                tasks: &b,
                selection: Selection::Explicit,
            },
            PlannedScanner {
                name: "a",
                tasks: &a,
                selection: Selection::Explicit,
            },
        ])
        .unwrap();
        assert_edge(&plan, "a:write", "b:read");
        let read = &plan.tasks[idx(&plan, "b:read")];
        assert_eq!(
            read.after,
            [Edge {
                upstream: idx(&plan, "a:write"),
                pattern: "finding".into(),
            }]
        );
        assert!(read.unmatched.is_empty());
        // Scanner order, not argument order
        assert_eq!(plan.tasks[0].label(), "a:write");
    }

    #[test]
    fn unmatched_wants_is_recorded_and_the_task_is_ready() {
        let a = tasks(&[("read", &["nobody-writes-this"], &[], &[])]);
        let plan = plan(&[PlannedScanner {
            name: "a",
            tasks: &a,
            selection: Selection::Explicit,
        }])
        .unwrap();
        assert_eq!(plan.deps, [0]);
        assert_eq!(plan.tasks[0].unmatched, ["nobody-writes-this"]);
    }

    #[test]
    fn required_by_orders_the_pulled_in_task_after_each_writer() {
        let a = tasks(&[("review", &[], &["finding.code"], &[])]);
        let b = tasks(&[("review", &[], &["finding.general"], &[])]);
        let c = tasks(&[("report", &["finding.*"], &[], &["finding.*"])]);
        let plan = plan(&[
            PlannedScanner {
                name: "a",
                tasks: &a,
                selection: Selection::Explicit,
            },
            PlannedScanner {
                name: "b",
                tasks: &b,
                selection: Selection::Explicit,
            },
            PlannedScanner {
                name: "c",
                tasks: &c,
                selection: Selection::RequiredBy,
            },
        ])
        .unwrap();
        assert_edge(&plan, "a:review", "c:report");
        assert_edge(&plan, "b:review", "c:report");
        let report = &plan.tasks[idx(&plan, "c:report")];
        assert_eq!(
            plan.deps[idx(&plan, "c:report")],
            2,
            "one edge per upstream task"
        );
        assert_eq!(report.selected, Selected::RequiredBy("finding.*".into()));
        assert_eq!(
            report.after.len(),
            2,
            "wants and required_by on the same pattern record one edge per writer"
        );
    }

    #[test]
    fn a_task_writing_what_it_wants_has_no_self_edge() {
        let a = tasks(&[("both", &["finding"], &["finding"], &[])]);
        let err = plan(&[PlannedScanner {
            name: "a",
            tasks: &a,
            selection: Selection::Explicit,
        }]);
        // The want is matched by its own write, so it is not unmatched,
        // and the self edge is dropped
        let plan = err.unwrap();
        assert_eq!(plan.deps, [0]);
        assert!(plan.downstream[0].is_empty());
        assert!(plan.tasks[0].unmatched.is_empty());
    }

    #[test]
    fn a_cycle_is_a_plan_error_naming_the_tasks_on_it() {
        let a = tasks(&[("one", &["y"], &["x"], &[])]);
        let b = tasks(&[("two", &["x"], &["y"], &[])]);
        let c = tasks(&[("three", &["y"], &[], &[])]);
        let err = plan(&[
            PlannedScanner {
                name: "a",
                tasks: &a,
                selection: Selection::Explicit,
            },
            PlannedScanner {
                name: "b",
                tasks: &b,
                selection: Selection::Explicit,
            },
            PlannedScanner {
                name: "c",
                tasks: &c,
                selection: Selection::Explicit,
            },
        ])
        .err()
        .unwrap();
        assert_eq!(
            err,
            PlanError::Cycle(vec!["a:one".into(), "b:two".into()]),
            "c:three is downstream of the cycle, not on it"
        );
    }

    #[test]
    fn plan_json_records_selection_edges_and_unmatched_patterns() {
        let a = tasks(&[
            ("summarize", &[], &["project-summary.rules"], &[]),
            (
                "review",
                &["project-summary.*", "missing"],
                &["finding.code"],
                &[],
            ),
        ]);
        let c = tasks(&[("report", &[], &[], &["finding.*"])]);
        let plan = plan(&[
            PlannedScanner {
                name: "a",
                tasks: &a,
                selection: Selection::Group("default".into()),
            },
            PlannedScanner {
                name: "c",
                tasks: &c,
                selection: Selection::RequiredBy,
            },
        ])
        .unwrap();
        assert_eq!(
            plan.to_json(),
            serde_json::json!({
                "tasks": [
                    {
                        "task": "a:review",
                        "selected": "group:default",
                        "after": [
                            { "task": "a:summarize", "pattern": "project-summary.*" }
                        ],
                        "unmatched": ["missing"],
                        "writes": { "finding.code": "" }
                    },
                    {
                        "task": "a:summarize",
                        "selected": "group:default",
                        "after": [],
                        "unmatched": [],
                        "writes": { "project-summary.rules": "" }
                    },
                    {
                        "task": "c:report",
                        "selected": "required_by:finding.*",
                        "after": [
                            { "task": "a:review", "pattern": "finding.*" }
                        ],
                        "unmatched": [],
                        "writes": {}
                    }
                ]
            })
        );
    }
}
