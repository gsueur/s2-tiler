/// Axum HTTP server: tile endpoint, TileJSON, config/info/build routes.
use crate::{
    cache::{TileCache, TileKey},
    cog::CogReader,
    config::S2Config,
    index::{build_index, save_index, MosaicIndex},
    pipeline::render_tile,
    render::{empty_tile_png, encode_tile, OutputFormat},
    stac::search_items,
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderValue, header, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use tower_http::cors::CorsLayer;
use bytes::Bytes;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{OnceCell, RwLock};
use tracing::{error, info};

// ─── State ──────────────────────────────────────────────────────────────────

pub struct TilesetState {
    pub config: S2Config,
    pub index: RwLock<Arc<MosaicIndex>>,
}

pub struct AppState {
    pub tilesets: HashMap<String, TilesetState>,
    pub cog_reader: CogReader,
    pub http_client: reqwest::Client,
    pub port: u16,
    pub index_db_path: Option<String>,
    pub tile_cache: Arc<dyn TileCache>,
    /// Optional public base URL for TileJSON (e.g. "https://tiles.example.com").
    pub public_url: Option<String>,
    /// Optional Cache-Control max-age in seconds for tile responses.
    pub cache_max_age: Option<u64>,
    /// Singleflight map: tile cache key → in-flight render result.
    pub in_flight: Arc<DashMap<String, Arc<OnceCell<Option<Bytes>>>>>,
}

// ─── Router ──────────────────────────────────────────────────────────────────

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/tilesets", get(list_handler))
        .route("/{name}/{z}/{x}/{y}", get(tile_handler))
        .route("/{name}/tilejson.json", get(tilejson_handler))
        .route("/{name}/config", get(config_handler))
        .route("/{name}/info", get(info_handler))
        .route("/{name}/build", post(build_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn tileset_or_404<'a>(
    state: &'a AppState,
    name: &str,
) -> Result<&'a TilesetState, Response> {
    state.tilesets.get(name).ok_or_else(|| {
        (StatusCode::NOT_FOUND, format!("tileset '{name}' not found")).into_response()
    })
}

#[derive(Deserialize)]
struct TileQuery {
    #[serde(default = "default_format")]
    format: String,
}

fn default_format() -> String {
    "png".to_string()
}

// ─── GET /tilesets ───────────────────────────────────────────────────────────

async fn list_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut names: Vec<String> = state.tilesets.keys().cloned().collect();
    names.sort();
    Json(names)
}

// ─── GET /{name}/{z}/{x}/{y} ─────────────────────────────────────────────────

/// Build a tile response with correct Content-Type and optional Cache-Control header.
fn tile_response(bytes: Bytes, format: OutputFormat, cache_max_age: Option<u64>) -> Response {
    let mut resp = ([(header::CONTENT_TYPE, format.content_type())], bytes).into_response();
    if let Some(max_age) = cache_max_age {
        if let Ok(val) = HeaderValue::from_str(&format!("public, max-age={max_age}")) {
            resp.headers_mut().insert(header::CACHE_CONTROL, val);
        }
    }
    resp
}

async fn tile_handler(
    State(state): State<Arc<AppState>>,
    Path((name, z, x, y)): Path<(String, u8, u32, u32)>,
    Query(params): Query<TileQuery>,
) -> Response {
    let ts = match tileset_or_404(&state, &name) {
        Ok(ts) => ts,
        Err(r) => return r,
    };
    let config = &ts.config;

    if z < config.minzoom || z > config.maxzoom {
        return (StatusCode::NOT_FOUND, "zoom out of range").into_response();
    }

    let format = match OutputFormat::from_str(&params.format) {
        Some(f) => f,
        None => {
            return (StatusCode::BAD_REQUEST, "unsupported format; use png, jpg, or webp").into_response()
        }
    };

    let cache_key = TileKey::new(&name, z, x, y, &params.format);

    // Check the persistent tile cache first.
    if let Ok(Some(cached)) = state.tile_cache.get(&cache_key).await {
        return tile_response(cached, format, state.cache_max_age);
    }

    // Singleflight: if a concurrent request is already rendering this tile,
    // wait for its OnceCell to resolve rather than starting a duplicate render.
    let flight_key = cache_key.to_path();
    let cell = state
        .in_flight
        .entry(flight_key.clone())
        .or_insert_with(|| Arc::new(OnceCell::new()))
        .clone();

    let config_clone = config.clone();
    let cog_reader = state.cog_reader.clone();
    let tile_cache = state.tile_cache.clone();
    let cache_key_clone = cache_key.clone();
    let name_clone = name.clone();

    let index = ts.index.read().await;
    let index_clone = Arc::clone(&*index);
    drop(index);

    let result: &Option<Bytes> = cell
        .get_or_init(|| async move {
            let tile_result =
                render_tile(z, x, y, &config_clone, &index_clone, &cog_reader).await;

            match tile_result {
                Ok(Some(tile)) => match encode_tile(&tile, config_clone.rescale, format) {
                    Ok(bytes) => {
                        let _ = tile_cache.put(&cache_key_clone, bytes.clone()).await;
                        Some(bytes)
                    }
                    Err(e) => {
                        error!("Encode error for {name_clone}/{z}/{x}/{y}: {e:#}");
                        None
                    }
                },
                Ok(None) => None,
                Err(e) => {
                    error!("Render error for {name_clone}/{z}/{x}/{y}: {e:#}");
                    None
                }
            }
        })
        .await;

    // Clean up the in-flight entry. The cell value remains valid because
    // all concurrent waiters have already received it via get_or_init.
    state.in_flight.remove(&flight_key);

    match result {
        Some(bytes) => tile_response(bytes.clone(), format, state.cache_max_age),
        None => ([(header::CONTENT_TYPE, "image/png")], empty_tile_png()).into_response(),
    }
}

// ─── GET /{name}/tilejson.json ───────────────────────────────────────────────

#[derive(Serialize)]
struct TileJson {
    tilejson: &'static str,
    name: String,
    tiles: Vec<String>,
    minzoom: u8,
    maxzoom: u8,
    bounds: [f64; 4],
    center: [f64; 3],
}

async fn tilejson_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let ts = match tileset_or_404(&state, &name) {
        Ok(ts) => ts,
        Err(r) => return r,
    };
    let c = &ts.config;
    let [west, south, east, north] = c.extent;
    let center_zoom = (c.minzoom + c.maxzoom) / 2;

    let base = state
        .public_url
        .as_deref()
        .map(|u| u.trim_end_matches('/').to_string())
        .unwrap_or_else(|| format!("http://localhost:{}", state.port));

    Json(TileJson {
        tilejson: "2.2.0",
        name: name.clone(),
        tiles: vec![format!("{base}/{}/{{z}}/{{x}}/{{y}}?format=png", name)],
        minzoom: c.minzoom,
        maxzoom: c.maxzoom,
        bounds: [west, south, east, north],
        center: [(west + east) / 2.0, (south + north) / 2.0, center_zoom as f64],
    })
    .into_response()
}

// ─── GET /{name}/config ──────────────────────────────────────────────────────

async fn config_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let ts = match tileset_or_404(&state, &name) {
        Ok(ts) => ts,
        Err(r) => return r,
    };
    Json(serde_json::to_value(&ts.config).unwrap_or_default()).into_response()
}

// ─── GET /{name}/info ────────────────────────────────────────────────────────

#[derive(Serialize)]
struct InfoResponse {
    scene_count: usize,
    index_cells: usize,
    quadkey_zoom: u8,
    extent: [f64; 4],
    minzoom: u8,
    maxzoom: u8,
    bands: Vec<String>,
    composite: String,
}

async fn info_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let ts = match tileset_or_404(&state, &name) {
        Ok(ts) => ts,
        Err(r) => return r,
    };
    let index = ts.index.read().await;
    let c = &ts.config;
    Json(InfoResponse {
        scene_count: index.scene_count(),
        index_cells: index.index_cell_count(),
        quadkey_zoom: index.quadkey_zoom,
        extent: c.extent,
        minzoom: c.minzoom,
        maxzoom: c.maxzoom,
        bands: c.bands.clone(),
        composite: format!("{:?}", c.composite),
    })
    .into_response()
}

// ─── POST /{name}/build ──────────────────────────────────────────────────────

async fn build_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let ts = match tileset_or_404(&state, &name) {
        Ok(ts) => ts,
        Err(r) => return r,
    };

    info!("Rebuilding index for tileset '{name}'...");
    match search_items(&ts.config, &state.http_client).await {
        Ok(items) => {
            let new_index = Arc::new(build_index(&items, &ts.config));
            let n = new_index.scene_count();

            if let Some(path) = &state.index_db_path {
                if let Err(e) = save_index(&new_index, &name, path) {
                    error!("Failed to save index for '{name}': {e:#}");
                }
            }

            *ts.index.write().await = new_index;
            let _ = state.tile_cache.clear_tileset(&name).await;
            (StatusCode::OK, format!("'{name}' rebuilt: {n} scenes")).into_response()
        }
        Err(e) => {
            error!("Build failed for '{name}': {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}
