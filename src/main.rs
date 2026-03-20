mod cog;
mod composite;
mod config;
mod geo;
mod index;
mod pipeline;
mod render;
mod server;
mod stac;

use crate::{
    cog::CogReader,
    config::S2Config,
    index::{build_index, load_index, save_index},
    server::{build_router, AppState},
    stac::search_items,
};
use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Standalone Sentinel-2 async tile server (no GDAL, no Python)
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Path to the YAML config file
    #[arg(short, long, default_value = "config.yaml")]
    config: String,

    /// Path to persist the mosaic index (JSON); reloaded on restart if present
    #[arg(short, long)]
    index_path: Option<String>,

    /// Force rebuild the index even if an index file exists
    #[arg(long, default_value_t = false)]
    rebuild: bool,

    /// Bind address
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logging
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "s2_tiler=info".into()))
        .init();

    let cli = Cli::parse();

    // Load config
    let config = S2Config::from_yaml_file(&cli.config)?;
    info!(
        "Loaded config: extent={:?} years={:?} bands={:?}",
        config.extent, config.years, config.bands
    );

    // Build or load mosaic index
    let http_client = reqwest::Client::builder()
        .user_agent("s2-tiler/0.1")
        .build()?;

    let index = if !cli.rebuild {
        if let Some(path) = &cli.index_path {
            match load_index(path) {
                Ok(idx) => {
                    info!("Loaded index from {path}: {} scenes", idx.scene_count());
                    idx
                }
                Err(_) => {
                    info!("Index file not found or invalid — building from STAC...");
                    build_fresh_index(&config, &http_client, cli.index_path.as_deref()).await?
                }
            }
        } else {
            build_fresh_index(&config, &http_client, None).await?
        }
    } else {
        build_fresh_index(&config, &http_client, cli.index_path.as_deref()).await?
    };

    let port = config.port;
    let host = cli.host.clone();

    let state = Arc::new(AppState {
        config,
        index: RwLock::new(Arc::new(index)),
        cog_reader: CogReader::new(),
        http_client,
        index_path: cli.index_path,
    });

    let router = build_router(state);
    let addr = format!("{host}:{port}");
    info!("Listening on http://{addr}");
    info!("  Tile URL:    http://{addr}/{{z}}/{{x}}/{{y}}?format=png");
    info!("  TileJSON:    http://{addr}/tilejson.json");
    info!("  Info:        http://{addr}/info");
    info!("  Rebuild:     POST http://{addr}/build");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router).await?;

    Ok(())
}

async fn build_fresh_index(
    config: &S2Config,
    client: &reqwest::Client,
    save_path: Option<&str>,
) -> Result<index::MosaicIndex> {
    let items = search_items(config, client).await?;
    let idx = build_index(&items, config);
    if let Some(path) = save_path {
        save_index(&idx, path)?;
        info!("Index saved to {path}");
    }
    Ok(idx)
}
