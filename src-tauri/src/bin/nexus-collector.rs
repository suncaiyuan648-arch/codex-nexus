//! OS-managed Codex Nexus collector.

fn main() {
    let database = codex_nexus_lib::usage::collector_service::default_database_path();
    let once = std::env::args().any(|argument| argument == "--once");
    match codex_nexus_lib::usage::collector_service::StandaloneCollector::start(database)
        .and_then(|collector| collector.run(once))
    {
        Ok(()) => {}
        Err(error) => {
            eprintln!("nexus-collector: {error}");
            std::process::exit(1);
        }
    }
}
