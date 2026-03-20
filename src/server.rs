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
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
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
            return (StatusCode::BAD_REQUEST, "unsupported format; use png or jpg").into_response()
        }
    };

    let cache_key = TileKey::new(&name, z, x, y, &params.format);
    if let Ok(Some(cached)) = state.tile_cache.get(&cache_key).await {
        return (
            [(header::CONTENT_TYPE, format.content_type())],
            cached,
        )
            .into_response();
    }

    let index = ts.index.read().await;
    let tile_result = render_tile(z, x, y, config, &index, &state.cog_reader).await;

    match tile_result {
        Ok(Some(tile)) => match encode_tile(&tile, config.rescale, format) {
            Ok(bytes) => {
                let _ = state.tile_cache.put(&cache_key, bytes.clone()).await;
                ([(header::CONTENT_TYPE, format.content_type())], bytes).into_response()
            }
            Err(e) => {
                error!("Encode error for {name}/{z}/{x}/{y}: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "encode error").into_response()
            }
        },
        Ok(None) => (
            [(header::CONTENT_TYPE, "image/png")],
            empty_tile_png(),
        )
            .into_response(),
        Err(e) => {
            error!("Render error for {name}/{z}/{x}/{y}: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
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

    Json(TileJson {
        tilejson: "2.2.0",
        name: name.clone(),
        tiles: vec![format!(
            "http://localhost:{}/{}/{{z}}/{{x}}/{{y}}?format=png",
            state.port, name
        )],
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
