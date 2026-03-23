/// Tile rendering pipeline: given (z, x, y), produce a composited image.
/// Orchestrates: index lookup → parallel COG reads → warp → SCL mask → composite.
use crate::{
    cog::CogReader,
    composite::{apply_scl_mask, composite, fill_gaps, SceneTile},
    config::S2Config,
    geo::{
        precompute_wm_to_utm_grid, warp_band_with_grid, warp_scl_with_grid,
        webmercator_bbox_to_utm, xyz_to_webmercator,
    },
    index::{MosaicIndex, SceneRef},
};
use anyhow::Result;
use futures::future::join_all;
use ndarray::Array3;
use tracing::debug;

pub const TILE_SIZE: u32 = 256;

/// Render a single XYZ tile to a composited `SceneTile` (256×256, bands × height × width).
pub async fn render_tile(
    z: u8,
    x: u32,
    y: u32,
    config: &S2Config,
    index: &MosaicIndex,
    cog_reader: &CogReader,
) -> Result<Option<SceneTile>> {
    let tile_bbox_wm = xyz_to_webmercator(z, x, y);

    // Desired GSD: tile width in meters / output pixels
    // At tile zoom level z, tile width ≈ 40075km / 2^z
    let desired_gsd = tile_bbox_wm.width() / TILE_SIZE as f64;

    // At low zoom, one tile spans multiple S2 orbital tracks. Scale the scene
    // limit so we sample enough scenes to cover the full tile footprint.
    // Each factor-of-2 zoom step halves the tile area; cap growth at 4× the config limit.
    let zoom_scale = 1usize << (config.maxzoom.saturating_sub(z).min(2) as usize);
    let effective_max = (config.max_scenes_per_tile * zoom_scale).min(config.max_scenes_per_tile * 4);
    let scenes = index.scenes_for_tile(z, x, y, effective_max);

    if scenes.is_empty() {
        debug!("No scenes for tile {z}/{x}/{y}");
        return Ok(None);
    }

    debug!(
        "Rendering tile {z}/{x}/{y}: {} scenes, desired_gsd={desired_gsd:.1}m",
        scenes.len()
    );

    // Process each scene concurrently
    let scene_count = scenes.len();
    let futures: Vec<_> = scenes
        .into_iter()
        .map(|scene| {
            let tile_bbox_wm = tile_bbox_wm; // Copy (it's Copy)
            let cog_reader = cog_reader.clone();
            let bands = config.bands.clone();
            let scene = scene.clone();
            let desired_gsd = desired_gsd;
            let haze_dn_max = config.haze_dn_max;
            let scl_masking = config.scl_masking;
            async move {
                match render_scene(&scene, &tile_bbox_wm, &bands, desired_gsd, haze_dn_max, scl_masking, &cog_reader).await {
                    Ok(Some(t)) => Some(t),
                    Ok(None) => None,
                    Err(e) => {
                        debug!("Scene {} failed: {e:#}", scene.id);
                        None
                    }
                }
            }
        })
        .collect();

    let scene_tiles: Vec<SceneTile> = join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect();

    if scene_tiles.is_empty() {
        debug!("Tile {z}/{x}/{y}: all {scene_count} scenes failed or produced no valid pixels");
        return Ok(None);
    }

    let mut result = composite(scene_tiles, &config.composite, config.bands.len());
    fill_gaps(&mut result);
    Ok(Some(result))
}

/// Read + warp + mask a single scene for a tile.
async fn render_scene(
    scene: &SceneRef,
    tile_bbox_wm: &crate::geo::Bbox,
    band_codes: &[String],
    desired_gsd: f64,
    haze_dn_max: u16,
    scl_masking: bool,
    cog_reader: &CogReader,
) -> Result<Option<SceneTile>> {
    let epsg = scene.epsg;

    // Convert tile bbox from WebMercator → scene UTM, with a small buffer
    let utm_bbox = webmercator_bbox_to_utm(tile_bbox_wm, epsg)?;

    // Enlarge the UTM window by ~20% to avoid edge artefacts from the warp
    let buf_x = utm_bbox.width() * 0.1;
    let buf_y = utm_bbox.height() * 0.1;
    let utm_bbox_buf = crate::geo::Bbox {
        x_min: utm_bbox.x_min - buf_x,
        y_min: utm_bbox.y_min - buf_y,
        x_max: utm_bbox.x_max + buf_x,
        y_max: utm_bbox.y_max + buf_y,
    };

    // Read spectral bands (and optionally SCL) concurrently.
    // SCL is skipped when scl_masking is false, saving one HTTP request per scene.
    let bands_n = band_codes.len();

    let band_futs: Vec<_> = band_codes
        .iter()
        .map(|band_code| {
            let url = scene.band_urls.get(band_code).cloned();
            let reader = cog_reader.clone();
            let bbox = utm_bbox_buf;
            let gsd = desired_gsd;
            async move {
                match url {
                    Some(u) => reader.read_window_u16(&u, &bbox, gsd).await,
                    None => Err(anyhow::anyhow!("missing band URL for {band_code}")),
                }
            }
        })
        .collect();

    let (scl_opt, band_results) = if scl_masking {
        let scl_fut = {
            let url = scene.scl_url.clone();
            let reader = cog_reader.clone();
            let bbox = utm_bbox_buf;
            let scl_gsd = desired_gsd.max(20.0);
            async move { reader.read_window_u8(&url, &bbox, scl_gsd).await }
        };
        let (scl_result, band_results) = tokio::join!(scl_fut, join_all(band_futs));
        let scl = match scl_result {
            Ok(r) => r,
            Err(e) => {
                debug!("Failed to read SCL for scene {}: {e:#}", scene.id);
                return Ok(None);
            }
        };
        (Some(scl), band_results)
    } else {
        (None, join_all(band_futs).await)
    };

    // All I/O is done. Offload the CPU-heavy warp + mask work to the blocking thread pool
    // so we don't starve tokio's async I/O polling on worker threads.
    let tile_bbox_wm = *tile_bbox_wm;
    let band_codes_owned: Vec<String> = band_codes.to_vec();
    let scene_id = scene.id.clone();

    let scene_tile = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<SceneTile>> {
        // Precompute WebMercator→UTM grid once for this scene (shared across SCL + all bands)
        let utm_grid = precompute_wm_to_utm_grid(&tile_bbox_wm, epsg, TILE_SIZE)?;

        // Warp SCL if present
        let scl_warped = scl_opt.map(|(arr, affine)| {
            warp_scl_with_grid(&arr, &affine, &utm_grid, TILE_SIZE)
        });

        // Collect band arrays and warp each to 256×256 WebMercator
        let mut band_arrays = Vec::with_capacity(bands_n);
        for (i, result) in band_results.into_iter().enumerate() {
            match result {
                Ok((arr, affine)) => {
                    let warped = warp_band_with_grid(&arr, &affine, &utm_grid, TILE_SIZE);
                    band_arrays.push(warped);
                }
                Err(e) => {
                    debug!(
                        "Failed to read band {} for scene {scene_id}: {e:#}",
                        band_codes_owned[i]
                    );
                    return Ok(None);
                }
            }
        }

        if band_arrays.is_empty() {
            return Ok(None);
        }

        // Assemble bands into Array3
        let n = TILE_SIZE as usize;
        let mut data = Array3::<u16>::zeros((bands_n, n, n));
        for (b, band_arr) in band_arrays.iter().enumerate() {
            for r in 0..n {
                for c in 0..n {
                    data[[b, r, c]] = band_arr[[r, c]];
                }
            }
        }

        let scene_tile = apply_scl_mask(data, scl_warped.as_ref().map(|a| a as &_), haze_dn_max);

        if !scene_tile.mask.iter().any(|&v| v) {
            let covered = scene_tile.covered.iter().filter(|&&v| v).count();
            tracing::debug!(
                "Scene {scene_id}: no valid pixels after masking \
                 (covered={covered}/65536, scl_masking={scl_masking}, haze_dn_max={haze_dn_max})"
            );
            return Ok(None);
        }

        Ok(Some(scene_tile))
    })
    .await??;

    Ok(scene_tile)
}
