use std::collections::HashMap;

use super::edge::EdgeType;
use petgraph::{
    dot::{Config, Dot},
    graph::NodeIndex,
    Graph,
};

pub struct GraphBuilder<'a> {
    graph: Graph<&'a str, &'a str>,
    nodes: HashMap<&'a str, NodeIndex>,
}

impl<'a> GraphBuilder<'a> {
    pub fn new() -> Self {
        let graph = Graph::<&str, &str>::new();
        let nodes = HashMap::new();

        Self { graph, nodes }
    }

    pub fn add_deps(&mut self, deps: &'a Vec<(String, String)>) {
        for (src, dst) in deps {
            let src = *self
                .nodes
                .entry(&src)
                .or_insert_with(|| self.graph.add_node(&src));
            let dst = *self
                .nodes
                .entry(&dst)
                .or_insert_with(|| self.graph.add_node(&dst));
            self.graph.add_edge(src, dst, "");
        }
    }

    pub fn dot(&mut self) {
        println!(
            "{:?}",
            Dot::with_config(&self.graph, &[Config::EdgeNoLabel])
        );
    }
}
