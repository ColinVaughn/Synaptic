use thiserror::Error;

/// Errors from the graph-assembly layer.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum GraphError {
    /// A rebuild/merge would shrink the graph below the previous node count.
    #[error("build would shrink graph from {existing} to {new} nodes; pass force to override")]
    Shrink { existing: usize, new: usize },
}
