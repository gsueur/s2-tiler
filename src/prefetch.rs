/// Chunk-based tile pre-seeding.
///
/// Instead of one COG HTTP read per output tile, tiles are grouped into spatial chunks
/// (CHUNK_TILES × CHUNK_TILES). For each chunk we issue ONE read per scene per band
/// covering the full chunk extent, then warp/composite/encode every tile from that
/// in-memory array — eliminating the O(tiles × scenes × bands) request pattern.
use crate::{
    cache::{TileCache, TileKey},
    cog::CogReader,
    composite::{apply_scl_mask, composite, fill_gaps},
    config::S2Config,
    geo::{
        precompute_wm_to_utm_grid, warp_band_with_grid, warp_scl_with_grid,
        webmercator_bbox_to_utm, webmercator_bbox_to_wgs84, wgs84_to_tile, xyz_to_webmercator, Bbox,
    },
    index::{MosaicIndex, SceneRef},
    pipeline::TILE_SIZE,
    render::{encode_tile, OutputFormat},
};
use anyhow::Result;
use bytes::Bytes;
use futures::{
    future::join_all,
    stream::{self, StreamExt},
};
use crate::geo::Affine;
use indicatif::{ProgressBar, ProgressStyle};
use ndarray::{Array2, Array3};
use std::sync::Arc;
use tracing::warn;

/// Side length (in output tiles) of each prefetch chunk.
const CHUNK_TILES: u32 = 8; // 8×8 = 64 tiles per chunk

// ─── Geometry helpers ────────────────────────────────────────────────────────

/// All (x, y) tile coordinates at zoom z covering `extent`.
fn tiles_for_zoom(extent: [f64; 4], z: u8) -> Vec<(u32, u32)> {
    let [west, south, east, north] = extent;
    let (x_min, y_min) = wgs84_to_tile(west, north, z);
    let (x_max, y_max) = wgs84_to_tile(east, south, z);
    let mut tiles = Vec::new();
    for x in x_min..=x_max {
        for y in y_min..=y_max {
            tiles.push((x, y));
        }
    }
    tiles
}

/// Partition tiles at zoom z into CHUNK_TILES×CHUNK_TILES chunks.
/// Returns (x_min, y_min, x_max, y_max) per chunk (all inclusive).
fn chunks_for_zoom(extent: [f64; 4], z: u8) -> Vec<(u32, u32, u32, u32)> {
    let [west, south, east, north] = extent;
    let (tx_min, ty_min) = wgs84_to_tile(west, north, z);
    let (tx_max, ty_max) = wgs84_to_tile(east, south, z);
    let mut chunks = Vec::new();
    let mut x = tx_min;
    while x <= tx_max {
        let x_end = (x + CHUNK_TILES - 1).min(tx_max);
        let mut y = ty_min;
        while y <= ty_max {
            let y_end = (y + CHUNK_TILES - 1).min(ty_max);
            chunks.push((x, y, x_end, y_end));
            y = y_end + 1;
        }
        x = x_end + 1;
    }
    chunks
}

/// WebMercator bbox covering a full chunk (union of all its tile bboxes).
fn chunk_wm_bbox(z: u8, x_min: u32, y_min: u32, x_max: u32, y_max: u32) -> Bbox {
    let tl = xyz_to_webmercator(z, x_min, y_min);
    let br = xyz_to_webmercator(z, x_max, y_max);
    Bbox {
        x_min: tl.x_min,
        y_min: br.y_min,
        x_max: br.x_max,
        y_max: tl.y_max,
    }
}

// ─── Chunk render ────────────────────────────────────────────────────────────

/// Pre-fetched data for one scene within a chunk.
struct LoadedScene {
    /// One (array, affine) per spectral band — covers the full chunk extent.
    bands: Vec<(Array2<u16>, Affine)>,
    scl: Array2<u8>,
    scl_affine: Affine,
    epsg: u32,
}

/// HTTP phase: read COG data for every scene covering this chunk.
/// Returns one `LoadedScene` per scene that could be read successfully.
async fn fetch_chunk_scenes(
    z: u8,
    x_min: u32,
    y_min: u32,
    x_max: u32,
    y_max: u32,
    scenes: &[&SceneRef],
    config: &S2Config,
    cog_reader: &CogReader,
) -> Vec<LoadedScene> {
    let desired_gsd = xyz_to_webmercator(z, x_min, y_min).width() / TILE_SIZE as f64;
    let scl_gsd = desired_gsd.max(20.0);
    let chunk_wm = chunk_wm_bbox(z, x_min, y_min, x_max, y_max);

    let mut loaded = Vec::new();

    for scene in scenes {
        let epsg = scene.epsg;

        let Ok(utm_bbox) = webmercator_bbox_to_utm(&chunk_wm, epsg) else {
            continue;
        };
        let buf_x = utm_bbox.width() * 0.1;
        let buf_y = utm_bbox.height() * 0.1;
        let utm_buf = Bbox {
            x_min: utm_bbox.x_min - buf_x,
            y_min: utm_bbox.y_min - buf_y,
            x_max: utm_bbox.x_max + buf_x,
            y_max: utm_bbox.y_max + buf_y,
        };

        // SCL and band reads concurrently
        let scl_fut = {
            let url = scene.scl_url.clone();
            let reader = cog_reader.clone();
            let bbox = utm_buf;
            async move { reader.read_window_u8(&url, &bbox, scl_gsd).await }
        };

        let band_futs: Vec<_> = config
            .bands
            .iter()
            .map(|band_code| {
                let url = scene.band_urls.get(band_code).cloned();
                let reader = cog_reader.clone();
                let bbox = utm_buf;
                let gsd = desired_gsd;
                async move {
                    match url {
                        Some(u) => reader.read_window_u16(&u, &bbox, gsd).await,
                        None => Err(anyhow::anyhow!("missing band URL")),
                    }
                }
            })
            .collect();

        let (scl_result, band_results) = tokio::join!(scl_fut, join_all(band_futs));

        let Ok((scl, scl_affine)) = scl_result else {
            continue;
        };

        let mut bands = Vec::with_capacity(config.bands.len());
        let mut all_ok = true;
        for r in band_results {
            match r {
                Ok(ab) => bands.push(ab),
                Err(_) => {
                    all_ok = false;
                    break;
                }
            }
        }
        if !all_ok {
            continue;
        }

        loaded.push(LoadedScene { bands, scl, scl_affine, epsg });
    }

    loaded
}

/// CPU phase (runs in spawn_blocking): warp + composite + encode all tiles in the chunk.
/// Returns a list of (TileKey, Bytes) for tiles that produced data.
fn render_tiles_from_memory(
    z: u8,
    x_min: u32,
    y_min: u32,
    x_max: u32,
    y_max: u32,
    loaded_scenes: Vec<LoadedScene>,
    config: Arc<S2Config>,
    format: OutputFormat,
) -> Vec<(TileKey, Bytes)> {
    let format_str = format.as_ext();
    let n_bands = config.bands.len();
    let n = TILE_SIZE as usize;
    let mut results = Vec::new();

    for x in x_min..=x_max {
        for y in y_min..=y_max {
            let tile_bbox_wm = xyz_to_webmercator(z, x, y);
            let mut scene_tiles = Vec::new();

            for ls in &loaded_scenes {
                let Ok(utm_grid) = precompute_wm_to_utm_grid(&tile_bbox_wm, ls.epsg, TILE_SIZE)
                else {
                    continue;
                };

                let scl_warped =
                    warp_scl_with_grid(&ls.scl, &ls.scl_affine, &utm_grid, TILE_SIZE);

                let mut data = Array3::<u16>::zeros((n_bands, n, n));
                for (b, (band_arr, band_affine)) in ls.bands.iter().enumerate() {
                    let warped = warp_band_with_grid(band_arr, band_affine, &utm_grid, TILE_SIZE);
                    for r in 0..n {
                        for c in 0..n {
                            data[[b, r, c]] = warped[[r, c]];
                        }
                    }
                }

                let scene_tile = apply_scl_mask(data, &scl_warped, config.haze_dn_max);
                if scene_tile.mask.iter().any(|&v| v) {
                    scene_tiles.push(scene_tile);
                }
            }

            if scene_tiles.is_empty() {
                continue;
            }

            let mut composited = composite(scene_tiles, &config.composite, config.bands.len());
            fill_gaps(&mut composited);

            if let Ok(bytes) = encode_tile(&composited, config.rescale, format) {
                let key = TileKey::new(&config.name, z, x, y, format_str);
                results.push((key, bytes));
            }
        }
    }

    results
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Pre-seed all tiles for a tileset into the cache.
///
/// Tiles are grouped into CHUNK_TILES×CHUNK_TILES spatial chunks. Each chunk issues
/// one COG read per scene per band (covering all chunk tiles at once), then warps and
/// composites every tile from memory — drastically reducing HTTP requests vs. per-tile reads.
pub async fn prefetch_tileset(
    config: &S2Config,
    index: &MosaicIndex,
    cog_reader: &CogReader,
    tile_cache: &Arc<dyn TileCache>,
    concurrency: usize,
    format: OutputFormat,
    skip_cached: bool,
    pb: ProgressBar,
) -> Result<()> {
    let format_str = format.as_ext();

    // Count total tiles for the progress bar
    let total_tiles: usize = (config.minzoom..=config.maxzoom)
        .map(|z| tiles_for_zoom(config.extent, z).len())
        .sum();

    let zoom_scale = 1usize << (config.maxzoom.saturating_sub(config.minzoom).min(2) as usize);
    let max_scenes = (config.max_scenes_per_tile * zoom_scale).min(config.max_scenes_per_tile * 4);

    pb.set_length(total_tiles as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{msg}\n[{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} ({percent}%) | {per_sec} tiles/s | eta {eta}",
        )
        .unwrap()
        .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    pb.set_message(format!(
        "[{}] seeding z{}–z{}, concurrency={} chunks",
        config.name, config.minzoom, config.maxzoom, concurrency
    ));

    let index = Arc::new(index.clone());
    let config = Arc::new(config.clone());

    let mut rendered = 0usize;
    let mut skipped = 0usize;
    let mut errors = 0usize;

    // Build the full list of chunks across all zoom levels (coarse-first)
    let all_chunks: Vec<(u8, u32, u32, u32, u32)> = (config.minzoom..=config.maxzoom)
        .flat_map(|z| {
            chunks_for_zoom(config.extent, z)
                .into_iter()
                .map(move |(x0, y0, x1, y1)| (z, x0, y0, x1, y1))
        })
        .collect();

    let mut stream = stream::iter(all_chunks)
        .map(|(z, x_min, y_min, x_max, y_max)| {
            let cog_reader = cog_reader.clone();
            let index = index.clone();
            let config = config.clone();
            let tile_cache = tile_cache.clone();

            tokio::spawn(async move {
                let chunk_wm = chunk_wm_bbox(z, x_min, y_min, x_max, y_max);
                let wgs84 = webmercator_bbox_to_wgs84(&chunk_wm);
                let scene_refs = index.scenes_for_bbox(wgs84, max_scenes);

                // --- skip_cached check: if all tiles in the chunk are already cached, skip ---
                let tile_count = ((x_max - x_min + 1) * (y_max - y_min + 1)) as usize;

                if skip_cached {
                    let mut all_cached = true;
                    'outer: for x in x_min..=x_max {
                        for y in y_min..=y_max {
                            let key = TileKey::new(&config.name, z, x, y, format_str);
                            match tile_cache.get(&key).await {
                                Ok(Some(_)) => {}
                                _ => {
                                    all_cached = false;
                                    break 'outer;
                                }
                            }
                        }
                    }
                    if all_cached {
                        return (0usize, tile_count, 0usize);
                    }
                }

                if scene_refs.is_empty() {
                    return (0, tile_count, 0);
                }

                // HTTP phase: read COG data for the full chunk extent
                let loaded_scenes =
                    fetch_chunk_scenes(z, x_min, y_min, x_max, y_max, &scene_refs, &config, &cog_reader).await;

                if loaded_scenes.is_empty() {
                    return (0, tile_count, 0);
                }

                // CPU phase (blocking): warp + composite + encode all tiles from memory
                let config_blocking = config.clone();
                let encoded = tokio::task::spawn_blocking(move || {
                    render_tiles_from_memory(
                        z,
                        x_min,
                        y_min,
                        x_max,
                        y_max,
                        loaded_scenes,
                        config_blocking,
                        format,
                    )
                })
                .await
                .unwrap_or_default();

                // Async phase: write to tile cache
                let written = encoded.len();
                let empty = tile_count - written;
                for (key, bytes) in encoded {
                    if tile_cache.put(&key, bytes).await.is_err() {
                        warn!("[{}] cache write failed for {}", config.name, key.to_path());
                    }
                }

                (written, empty, 0usize)
            })
        })
        .buffer_unordered(concurrency);

    while let Some(join_result) = stream.next().await {
        let (r, s, e) = join_result.unwrap_or((0, 0, 1));
        rendered += r;
        skipped += s;
        errors += e;
        let done = rendered + skipped + errors;
        pb.set_position(done.min(total_tiles) as u64);
        pb.set_message(format!(
            "[{}] seeding — {} cached, {} empty, {} errors",
            config.name, rendered, skipped, errors
        ));
    }

    pb.finish_with_message(format!(
        "[{}] done — {} cached, {} empty, {} errors",
        config.name, rendered, skipped, errors
    ));

    Ok(())
}
