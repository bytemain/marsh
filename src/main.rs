mod collector;
mod graph;
mod loader;
mod message;
mod service;
mod utils;

fn main() {
    let cwd = std::env::current_dir().unwrap();
    // get args
    let args: Vec<Box<std::path::Path>> = std::env::args()
        .map(|arg| std::path::Path::new(&arg).into())
        .collect();

    let mut collector = collector::CollectorService::default();

    let options = service::AnalyzeServiceOptions::new(cwd, args.clone()).with_cross_module(true);
    let ana_service = service::AnalyzeService::new(options);

    // Spawn linting in another thread so diagnostics can be printed immediately from diagnostic_service.run.
    rayon::spawn({
        let tx_error = collector.sender().clone();
        let lint_service = ana_service.clone();
        move || {
            lint_service.run(&tx_error);
        }
    });
    collector.start();

    let mut graph_builder = graph::GraphBuilder::new();

    graph_builder.add_deps(&collector.deps);
    graph_builder.dot();
}
