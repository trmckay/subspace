use crate::error::{Error, Result};

/// A rooted tree with parent-child relationships.
///
/// Implementations must be acyclic. Traversal assumes this and can loop forever on a cycle.
pub trait Tree {
    /// The node type stored in this tree.
    type Node: Eq;

    /// Returns the root node.
    fn root(&self) -> &Self::Node;
    /// Returns the parent of a node, if any.
    fn parent(&self, node: &Self::Node) -> Option<&Self::Node>;
    /// Returns the direct children of a node.
    fn children(&self, node: &Self::Node) -> Vec<&Self::Node>;
}

/// Walks the parent chain from `start` and returns the root: the first value for which `parent`
/// returns `None`.
///
/// Returns an error after 10000 steps, which indicates a cycle.
pub fn find_root<T: Eq + std::fmt::Debug>(start: T, parent: impl Fn(&T) -> Option<T>) -> Result<T> {
    let mut current = start;
    let mut depth: usize = 0;
    while let Some(next) = parent(&current) {
        depth += 1;
        if depth >= 10_000 {
            return Err(Error::CycleDetected(format!("{current:?}")));
        }
        current = next;
    }
    Ok(current)
}
