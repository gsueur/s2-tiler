/// Axum HTTP server: tile endpoint, TileJSON, config/info/build routes.
use crate::{
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
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};

// ─── App state ──────────────────────────────────────────────────────────────

pub struct AppState {
    pub config: S2Config,
    pub index: RwLock<Arc<MosaicIndex>>,
    pub cog_reader: CogReader,
    pub http_client: reqwest::Client,
    pub index_path: Option<String>,
}

// ─── Router ─────────────────────────────────────────────────────────────────

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/:z/:x/:y", get(tile_handler))
        .route("/tilejson.json", get(tilejson_handler))
        .route("/config", get(config_handler))
        .route("/info", get(info_handler))
        .route("/build", post(build_handler))
        .with_state(state)
}

// ─── Query params ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TileQuery {
    #[serde(default = "default_format")]
    format: String,
}

fn default_format() -> String {
    "png".to_string()
}

// ─── Tile handler ────────────────────────────────────────────────────────────

async fn tile_handler(
    State(state): State<Arc<AppState>>,
    Path((z, x, y)): Path<(u8, u32, u32)>,
    Query(params): Query<TileQuery>,
) -> Response {
    let config = &state.config;

    // Zoom range check
    if z < config.minzoom || z > config.maxzoom {
        return (StatusCode::NOT_FOUND, "zoom out of range").into_response();
    }

    let format = match OutputFormat::from_str(&params.format) {
        Some(f) => f,
        None => {
            return (StatusCode::BAD_REQUEST, "unsupported format; use png or jpg").into_response()
        }
    };

    let index = state.index.read().await;

    let tile_result =
        render_tile(z, x, y, config, &index, &state.cog_reader).await;

    match tile_result {
        Ok(Some(tile)) => {
            match encode_tile(&tile, config.rescale, format) {
                Ok(bytes) => (
                    [(header::CONTENT_TYPE, format.content_type())],
                    bytes,
                )
                    .into_response(),
                Err(e) => {
                    error!("Encode error for {z}/{x}/{y}: {e:#}");
                    (StatusCode::INTERNAL_SERVER_ERROR, "encode error").into_response()
                }
            }
        }
        Ok(None) => {
            // Tile outside extent or no scenes — return empty transparent PNG
            (
                [(header::CONTENT_TYPE, "image/png")],
                empty_tile_png(),
            )
                .into_response()
        }
        Err(e) => {
            error!("Render error for {z}/{x}/{y}: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

// ─── TileJSON handler ────────────────────────────────────────────────────────

#[derive(Serialize)]
struct TileJson {
    tilejson: &'static str,
    name: &'static str,
    tiles: Vec<String>,
    minzoom: u8,
    maxzoom: u8,
    bounds: [f64; 4],
    center: [f64; 3],
}

async fn tilejson_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let c = &state.config;
    let [west, south, east, north] = c.extent;
    let center_lon = (west + east) / 2.0;
    let center_lat = (south + north) / 2.0;
    let center_zoom = (c.minzoom + c.maxzoom) / 2;

    Json(TileJson {
        tilejson: "2.2.0",
        name: "s2-tiler",
        tiles: vec![format!("http://localhost:{}{{z}}/{{x}}/{{y}}?format=png", c.port)],
        minzoom: c.minzoom,
        maxzoom: c.maxzoom,
        bounds: [west, south, east, north],
        center: [center_lon, center_lat, center_zoom as f64],
    })
}

// ─── Config handler ──────────────────────────────────────────────────────────

async fn config_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::to_value(&state.config).unwrap_or_default())
}

// ─── Info handler ────────────────────────────────────────────────────────────

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

async fn info_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let index = state.index.read().await;
    let c = &state.config;
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
}

// ─── Build handler ───────────────────────────────────────────────────────────

async fn build_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    info!("Rebuilding mosaic index...");
    match search_items(&state.config, &state.http_client).await {
        Ok(items) => {
            let new_index = Arc::new(build_index(&items, &state.config));
            let n = new_index.scene_count();

            if let Some(path) = &state.index_path {
                if let Err(e) = save_index(&new_index, path) {
                    error!("Failed to save index: {e:#}");
                }
            }

            *state.index.write().await = new_index;
            (StatusCode::OK, format!("Index rebuilt: {n} scenes")).into_response()
        }
        Err(e) => {
            error!("Build failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}
