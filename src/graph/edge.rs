#[derive(Debug, Clone)]
pub enum EdgeType {
    /// Conditional jumps
    Jump,
    /// Normal control flow path
    Normal,
    /// Cyclic aka loops
    Backedge,
    /// Marks start of a function subgraph
    NewFunction,
    /// Finally
    Finalize,

    // misc edges
    Unreachable,
    /// Used to mark the end of a finalizer. It is an experimental approach might
    /// move to it's respective edge kind enum or get removed altogether.
    Join,
}