//! Workflow DAG data structure — dependency graph for workflow execution.
//!
//! The DAG (Directed Acyclic Graph) represents the compiled workflow:
//! - Nodes are workflow steps
//! - Edges are dependencies (parent → child means "parent must complete before child starts")
//!
//! The DAG supports:
//! - Topological sorting for sequential execution order
//! - Parallel level detection (steps at the same level can run concurrently)
//! - Path-based dependency queries
//! - Cycle detection

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::config::StepConfig;

/// A node in the workflow DAG — represents a compiled step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DagNode {
    /// The step ID (unique within the DAG).
    pub id: String,

    /// The step configuration.
    pub step: StepConfig,

    /// IDs of nodes this node depends on (must complete before this one).
    #[serde(default)]
    pub depends_on: Vec<String>,

    /// IDs of nodes that depend on this node (this must complete before them).
    #[serde(default)]
    pub children: Vec<String>,

    /// The parallel level (0 = root, 1 = first generation, etc.).
    #[serde(default)]
    pub level: usize,
}

/// The workflow DAG — compiled from WorkflowConfig.
///
/// Contains the full dependency graph and supports:
/// - Topological ordering
/// - Parallel level grouping
/// - Dependency queries
/// - Root/leaf detection
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowDag {
    /// The workflow name.
    pub name: String,

    /// All nodes in the DAG, keyed by step ID.
    pub nodes: HashMap<String, DagNode>,

    /// The execution levels (parallel groups).
    /// Level 0 contains root nodes (no dependencies),
    /// Level 1 contains nodes that depend only on Level 0, etc.
    pub levels: Vec<Vec<String>>,

    /// Root node IDs (no dependencies).
    pub roots: Vec<String>,

    /// Leaf node IDs (no children).
    pub leaves: Vec<String>,
}

impl WorkflowDag {
    /// Create an empty DAG.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            nodes: HashMap::new(),
            levels: Vec::new(),
            roots: Vec::new(),
            leaves: Vec::new(),
        }
    }

    /// Add a node to the DAG.
    pub fn add_node(&mut self, node: DagNode) {
        self.nodes.insert(node.id.clone(), node);
    }

    /// Build the dependency edges and compute levels.
    ///
    /// After all nodes are added, call this to:
    /// 1. Resolve `depends_on` → set `children` on parent nodes
    /// 2. Compute parallel levels via BFS
    /// 3. Identify root and leaf nodes
    pub fn build(&mut self) {
        // Build children from depends_on (collect first to avoid borrow conflict)
        let children_map: HashMap<String, Vec<String>> = {
            let mut map: HashMap<String, Vec<String>> = HashMap::new();
            for node in self.nodes.values() {
                for parent_id in &node.depends_on {
                    map.entry(parent_id.clone())
                        .or_default()
                        .push(node.id.clone());
                }
            }
            map
        };

        // Apply children to parent nodes
        for (parent_id, children) in &children_map {
            if let Some(parent) = self.nodes.get_mut(parent_id) {
                for child_id in children {
                    if !parent.children.contains(child_id) {
                        parent.children.push(child_id.clone());
                    }
                }
            }
        }

        // Find roots (nodes with no dependencies)
        self.roots = self.nodes.values()
            .filter(|n| n.depends_on.is_empty())
            .map(|n| n.id.clone())
            .collect();

        // Find leaves (nodes with no children)
        self.leaves = self.nodes.values()
            .filter(|n| n.children.is_empty())
            .map(|n| n.id.clone())
            .collect();

        // Compute parallel levels via BFS
        self.compute_levels();
    }

    /// Compute parallel levels via topological BFS.
    ///
    /// Each level contains nodes that can be executed concurrently
    /// (all their dependencies are in earlier levels).
    fn compute_levels(&mut self) {
        self.levels.clear();

        // Track which nodes have been assigned to a level
        let mut assigned: HashSet<String> = HashSet::new();
        let mut current_level: Vec<String> = self.roots.clone();

        if current_level.is_empty() && !self.nodes.is_empty() {
            // No roots — this means all nodes depend on something,
            // which is only possible if there are cycles (invalid DAG)
            return;
        }

        while !current_level.is_empty() {
            // Set level on nodes
            for node_id in &current_level {
                if let Some(node) = self.nodes.get_mut(node_id) {
                    node.level = self.levels.len();
                }
                assigned.insert(node_id.clone());
            }

            self.levels.push(current_level.clone());

            // Find next level: nodes whose all dependencies are in assigned
            let mut next_level: Vec<String> = Vec::new();
            for (id, node) in &self.nodes {
                if assigned.contains(id) {
                    continue;
                }
                // Check if all dependencies are assigned
                let all_deps_done = node.depends_on.iter()
                    .all(|dep| assigned.contains(dep));
                if all_deps_done {
                    next_level.push(id.clone());
                }
            }

            current_level = next_level;
        }
    }

    /// Get a node by ID.
    pub fn get_node(&self, id: &str) -> Option<&DagNode> {
        self.nodes.get(id)
    }

    /// Get the number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Get the number of levels (parallel groups).
    pub fn level_count(&self) -> usize {
        self.levels.len()
    }

    /// Get nodes at a specific level.
    pub fn level_nodes(&self, level: usize) -> Vec<&DagNode> {
        if level >= self.levels.len() {
            return Vec::new();
        }
        self.levels[level].iter()
            .filter_map(|id| self.nodes.get(id))
            .collect()
    }

    /// Get all node IDs in topological order (level 0 → level N).
    pub fn topological_order(&self) -> Vec<String> {
        self.levels.iter().flatten().cloned().collect()
    }

    /// Check if the DAG has any cycles.
    ///
    /// Uses Kahn's algorithm — if after removing all nodes with no
    /// dependencies, some nodes remain, there's a cycle.
    pub fn has_cycle(&self) -> bool {
        let mut in_degree: HashMap<String, usize> = self.nodes.keys()
            .map(|id| (id.clone(), 0))
            .collect();

        for node in self.nodes.values() {
            in_degree.insert(node.id.clone(), node.depends_on.len());
        }

        let mut queue: VecDeque<String> = in_degree.iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(id, _)| id.clone())
            .collect();

        let mut visited_count = 0;
        while let Some(id) = queue.pop_front() {
            visited_count += 1;
            if let Some(node) = self.nodes.get(&id) {
                for child_id in &node.children {
                    if let Some(deg) = in_degree.get_mut(child_id) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(child_id.clone());
                        }
                    }
                }
            }
        }

        visited_count != self.nodes.len()
    }

    /// Get all dependencies (transitive) of a node.
    pub fn transitive_deps(&self, node_id: &str) -> HashSet<String> {
        let mut deps = HashSet::new();
        let mut to_visit = VecDeque::new();

        if let Some(node) = self.nodes.get(node_id) {
            for dep_id in &node.depends_on {
                to_visit.push_back(dep_id.clone());
            }
        }

        while let Some(id) = to_visit.pop_front() {
            if deps.contains(&id) {
                continue;
            }
            deps.insert(id.clone());
            if let Some(node) = self.nodes.get(&id) {
                for dep_id in &node.depends_on {
                    if !deps.contains(dep_id) {
                        to_visit.push_back(dep_id.clone());
                    }
                }
            }
        }

        deps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StepConfig;

    fn make_step(id: &str, depends_on: Vec<&str>) -> StepConfig {
        StepConfig {
            id: id.to_string(),
            description: format!("Step {}", id),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            tool: None,
            tool_args: None,
            prompt: None,
            requires_approval: false,
            timeout_secs: None,
            retry_policy: None,
            metadata: HashMap::new(),
        }
    }

    fn make_node(id: &str, depends_on: Vec<&str>) -> DagNode {
        let depends_on_owned: Vec<String> = depends_on.iter().map(|s| s.to_string()).collect();
        DagNode {
            id: id.to_string(),
            step: make_step(id, depends_on),
            depends_on: depends_on_owned,
            children: Vec::new(),
            level: 0,
        }
    }

    #[test]
    fn test_simple_dag() {
        let mut dag = WorkflowDag::new("test");
        dag.add_node(make_node("a", vec![]));
        dag.add_node(make_node("b", vec!["a"]));
        dag.add_node(make_node("c", vec!["b"]));
        dag.build();

        assert_eq!(dag.node_count(), 3);
        assert_eq!(dag.level_count(), 3);
        assert_eq!(dag.roots, vec!["a"]);
        assert_eq!(dag.leaves, vec!["c"]);
        assert_eq!(dag.levels[0], vec!["a"]);
        assert_eq!(dag.levels[1], vec!["b"]);
        assert_eq!(dag.levels[2], vec!["c"]);
    }

    #[test]
    fn test_parallel_dag() {
        let mut dag = WorkflowDag::new("test");
        dag.add_node(make_node("a", vec![]));
        dag.add_node(make_node("b", vec![]));
        dag.add_node(make_node("c", vec!["a", "b"]));
        dag.build();

        assert_eq!(dag.level_count(), 2);
        // Level 0 contains both "a" and "b" (order depends on HashMap iteration)
        assert_eq!(dag.levels[0].len(), 2);
        assert!(dag.levels[0].contains(&"a".to_string()));
        assert!(dag.levels[0].contains(&"b".to_string()));
        assert_eq!(dag.levels[1], vec!["c"]);
        // Roots are a and b (order may vary)
        assert_eq!(dag.roots.len(), 2);
        assert!(dag.roots.contains(&"a".to_string()));
        assert!(dag.roots.contains(&"b".to_string()));
        assert_eq!(dag.leaves, vec!["c"]);
    }

    #[test]
    fn test_diamond_dag() {
        // a → b → d
        // a → c → d
        let mut dag = WorkflowDag::new("test");
        dag.add_node(make_node("a", vec![]));
        dag.add_node(make_node("b", vec!["a"]));
        dag.add_node(make_node("c", vec!["a"]));
        dag.add_node(make_node("d", vec!["b", "c"]));
        dag.build();

        assert_eq!(dag.level_count(), 3);
        assert_eq!(dag.levels[0], vec!["a"]);
        // Level 1 contains both "b" and "c" (order depends on HashMap)
        assert_eq!(dag.levels[1].len(), 2);
        assert!(dag.levels[1].contains(&"b".to_string()));
        assert!(dag.levels[1].contains(&"c".to_string()));
        assert_eq!(dag.levels[2], vec!["d"]);
    }

    #[test]
    fn test_dag_cycle_detection() {
        let mut dag = WorkflowDag::new("test");
        dag.add_node(make_node("a", vec!["b"]));
        dag.add_node(make_node("b", vec!["a"]));
        dag.build();

        assert!(dag.has_cycle());
    }

    #[test]
    fn test_dag_no_cycle() {
        let mut dag = WorkflowDag::new("test");
        dag.add_node(make_node("a", vec![]));
        dag.add_node(make_node("b", vec!["a"]));
        dag.build();

        assert!(!dag.has_cycle());
    }

    #[test]
    fn test_dag_topological_order() {
        let mut dag = WorkflowDag::new("test");
        dag.add_node(make_node("a", vec![]));
        dag.add_node(make_node("b", vec!["a"]));
        dag.add_node(make_node("c", vec!["a"]));
        dag.add_node(make_node("d", vec!["b", "c"]));
        dag.build();

        let order = dag.topological_order();
        assert_eq!(order.len(), 4);
        // a must come before b and c, which must come before d
        let a_pos = order.iter().position(|x| x == "a").unwrap();
        let b_pos = order.iter().position(|x| x == "b").unwrap();
        let c_pos = order.iter().position(|x| x == "c").unwrap();
        let d_pos = order.iter().position(|x| x == "d").unwrap();
        assert!(a_pos < b_pos);
        assert!(a_pos < c_pos);
        assert!(b_pos < d_pos);
        assert!(c_pos < d_pos);
    }

    #[test]
    fn test_dag_transitive_deps() {
        let mut dag = WorkflowDag::new("test");
        dag.add_node(make_node("a", vec![]));
        dag.add_node(make_node("b", vec!["a"]));
        dag.add_node(make_node("c", vec!["b"]));
        dag.build();

        let deps = dag.transitive_deps("c");
        assert!(deps.contains("a"));
        assert!(deps.contains("b"));
        assert_eq!(deps.len(), 2);
    }

    #[test]
    fn test_dag_level_nodes() {
        let mut dag = WorkflowDag::new("test");
        dag.add_node(make_node("a", vec![]));
        dag.add_node(make_node("b", vec!["a"]));
        dag.build();

        let level0 = dag.level_nodes(0);
        assert_eq!(level0.len(), 1);
        assert_eq!(level0[0].id, "a");

        let level1 = dag.level_nodes(1);
        assert_eq!(level1.len(), 1);
        assert_eq!(level1[0].id, "b");
    }
}