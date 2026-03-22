mod cache;
mod cog;
mod composite;
mod config;
mod geo;
mod index;
mod pipeline;
mod prefetch;
mod render;
mod server;
mod stac;

use crate::{
    cache::{DuckDbCache, LocalCache, MemoryCache, NoCache, ObjectStoreCache, TileCache},
    cog::CogReader,
    config::{AppConfig, S2Config, TileCacheConfig},
    index::{build_index, load_index, save_index, MosaicIndex},
    prefetch::prefetch_tileset,
    render::OutputFormat,
    server::{build_router, AppState, TilesetState},
    stac::search_items,
};
use anyhow::{Context, Result};
use clap::Parser;
use object_store::aws::AmazonS3Builder;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use indicatif::{MultiProgress, ProgressBar};
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Standalone Sentinel-2 async tile server (no GDAL, no Python)
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Path to the YAML config file
    #[arg(short, long, default_value = "config.yaml")]
    config: String,

    /// Force rebuild all indices even if index files exist
    #[arg(long, default_value_t = false)]
    rebuild: bool,

    /// Bind address
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Pre-render all tiles into the cache then exit (skips serving)
    #[arg(long, default_value_t = false)]
    prefetch: bool,

    /// Max concurrent tile renders during prefetch
    #[arg(long, default_value_t = 8)]
    concurrency: usize,

    /// Output format for prefetched tiles (png, jpg, webp)
    #[arg(long, default_value = "png")]
    prefetch_format: String,

    /// Skip tiles already present in the cache (useful for resuming an interrupted prefetch)
    #[arg(long, default_value_t = false)]
    skip_cached: bool,

    /// Tilesets to prefetch (by name); defaults to all tilesets if omitted
    #[arg(long, value_delimiter = ',')]
    tilesets: Vec<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "s2_tiler=info".into()))
        .init();

    let multi = MultiProgress::new();

    let cli = Cli::parse();
    let app_config = AppConfig::from_yaml_file(&cli.config)?;

    let http_client = reqwest::Client::builder()
        .user_agent("s2-tiler/0.1")
        .build()?;

    let mut tilesets: HashMap<String, TilesetState> = HashMap::new();

    for ts_config in app_config.tilesets {
        info!(
            "Tileset '{}': extent={:?} years={:?} bands={:?}",
            ts_config.name, ts_config.extent, ts_config.years, ts_config.bands
        );

        let index = if !cli.rebuild {
            if let Some(path) = &app_config.index_path {
                match load_index(&ts_config.name, path) {
                    Ok(idx) => {
                        info!(
                            "Loaded '{}' from {}: {} scenes",
                            ts_config.name,
                            path,
                            idx.scene_count()
                        );
                        idx
                    }
                    Err(_) => {
                        info!(
                            "Index not found — building '{}' from STAC...",
                            ts_config.name
                        );
                        build_fresh_index(&ts_config, &http_client, app_config.index_path.as_deref()).await?
                    }
                }
            } else {
                build_fresh_index(&ts_config, &http_client, None).await?
            }
        } else {
            build_fresh_index(&ts_config, &http_client, app_config.index_path.as_deref()).await?
        };

        tilesets.insert(
            ts_config.name.clone(),
            TilesetState {
                config: ts_config,
                index: RwLock::new(Arc::new(index)),
            },
        );
    }

    let port = app_config.port;
    let host = cli.host.clone();

    let tile_cache = build_tile_cache(&app_config.tile_cache)?;

    let state = Arc::new(AppState {
        tilesets,
        cog_reader: CogReader::new(),
        http_client,
        port,
        index_db_path: app_config.index_path,
        tile_cache,
        public_url: app_config.public_url,
        cache_max_age: app_config.cache_max_age,
        in_flight: Arc::new(dashmap::DashMap::new()),
    });

    if cli.prefetch {
        let format = OutputFormat::from_str(&cli.prefetch_format)
            .ok_or_else(|| anyhow::anyhow!("unsupported prefetch format: {}", cli.prefetch_format))?;

        let mut names: Vec<&str> = if cli.tilesets.is_empty() {
            state.tilesets.keys().map(String::as_str).collect()
        } else {
            for name in &cli.tilesets {
                anyhow::ensure!(
                    state.tilesets.contains_key(name.as_str()),
                    "unknown tileset '{name}'; available: {}",
                    state.tilesets.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
            cli.tilesets.iter().map(String::as_str).collect()
        };
        names.sort();

        for name in names {
            let ts = &state.tilesets[name];
            let index = ts.index.read().await;
            let pb = multi.add(ProgressBar::new(0));
            prefetch_tileset(
                &ts.config,
                &index,
                &state.cog_reader,
                &state.tile_cache,
                cli.concurrency,
                format,
                cli.skip_cached,
                pb,
            )
            .await?;
        }

        info!("Prefetch complete.");
        return Ok(());
    }

    let addr = format!("{host}:{port}");
    info!("Listening on http://{addr}");
    let mut names: Vec<&str> = state.tilesets.keys().map(String::as_str).collect();
    names.sort();
    for name in names {
        info!("  http://{addr}/{name}/{{z}}/{{x}}/{{y}}?format=png");
    }
    info!("  Tilesets:  http://{addr}/tilesets");

    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router).await?;

    Ok(())
}

fn build_tile_cache(cfg: &TileCacheConfig) -> Result<Arc<dyn TileCache>> {
    let cache: Arc<dyn TileCache> = match cfg {
        TileCacheConfig::None => Arc::new(NoCache),
        TileCacheConfig::Memory => Arc::new(MemoryCache::new()),
        TileCacheConfig::Local { path } => Arc::new(LocalCache::new(path)),
        TileCacheConfig::Duckdb { path } => Arc::new(DuckDbCache::new(path)?),
        TileCacheConfig::S3 { bucket, region, prefix } => {
            let store = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_region(region)
                .with_access_key_id(
                    std::env::var("AWS_ACCESS_KEY_ID")
                        .context("AWS_ACCESS_KEY_ID not set")?,
                )
                .with_secret_access_key(
                    std::env::var("AWS_SECRET_ACCESS_KEY")
                        .context("AWS_SECRET_ACCESS_KEY not set")?,
                )
                .build()?;
            Arc::new(ObjectStoreCache::new(store, prefix.as_deref().unwrap_or("tiles")))
        }
        TileCacheConfig::R2 { bucket, account_id, prefix } => {
            let store = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_endpoint(format!(
                    "https://{account_id}.r2.cloudflarestorage.com"
                ))
                .with_region("auto")
                .with_virtual_hosted_style_request(false)
                .with_access_key_id(
                    std::env::var("R2_ACCESS_KEY_ID")
                        .context("R2_ACCESS_KEY_ID not set")?,
                )
                .with_secret_access_key(
                    std::env::var("R2_SECRET_ACCESS_KEY")
                        .context("R2_SECRET_ACCESS_KEY not set")?,
                )
                .build()?;
            Arc::new(ObjectStoreCache::new(store, prefix.as_deref().unwrap_or("tiles")))
        }
    };
    Ok(cache)
}

async fn build_fresh_index(
    config: &S2Config,
    client: &reqwest::Client,
    index_db_path: Option<&str>,
) -> Result<MosaicIndex> {
    let items = search_items(config, client).await?;
    let idx = build_index(&items, config);
    if let Some(path) = index_db_path {
        save_index(&idx, &config.name, path)?;
        info!("'{}' saved to {path}", config.name);
    }
    Ok(idx)
}
