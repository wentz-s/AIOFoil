// main.rs - Entry point
mod aggregator;
mod apibay;
mod cache;
mod config;
mod knaben;
mod leetx;
mod nyaa;
mod sources;
mod tinfoil;
mod titledb;
mod torbox;
mod tgx;
mod torrents_csv;
mod rar;
mod zip;

use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config = config::Config::parse();

    match config.migrate_old_cache() {
        Ok(true) => info!("Moved the cache from ~/.cache/tinfoil-aggregator to {}", config.cache_dir().display()),
        Ok(false) => {}
        Err(e) => warn!("Couldn't move the old cache from ~/.cache/tinfoil-aggregator: {}", e),
    }

    let cache = match cache::Cache::new(&config.cache_file()).await {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to initialize cache: {}", e);
            std::process::exit(1);
        }
    };

    if config.torbox_api_key.is_none() {
        warn!("No TorBox API key (--torbox-api-key or TORBOX_API_KEY) - the game list will load but downloads won't work");
    }

    // Keep the game list fresh in the background; the server starts right away
    let titledb = titledb::SharedTitleDb::default();
    let aggregator = aggregator::Aggregator::new(&config, &cache, &titledb);
    tokio::spawn(aggregator.run());

    if let Err(e) = tinfoil::start_server(&config, &cache, &titledb).await {
        error!("Server failed: {}", e);
        std::process::exit(1);
    }
}
