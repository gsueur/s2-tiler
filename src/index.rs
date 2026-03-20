use crate::config::S2Config;
use crate::geo::{bbox_to_quadkeys, tile_to_covering_quadkeys};
use crate::stac::StacItem;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::info;

/// Reference to a single S2 scene for a given spatial extent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneRef {
    pub id: String,
    /// Band code → HTTPS URL for each spectral band
    pub band_urls: HashMap<String, String>,
    /// SCL band URL (for cloud masking)
    pub scl_url: String,
    /// Cloud cover percentage [0, 100]
    pub cloud_cover: f64,
    /// Scene datetime (ISO 8601)
    pub datetime: String,
    /// UTM EPSG code (e.g. 32631 for UTM31N)
    pub epsg: u32,
}

/// Spatial index: quadkey (u64) → ordered list of scenes (cloud cover ascending).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MosaicIndex {
    pub scenes: Vec<SceneRef>,
    /// quadkey → indices into `scenes` (ordered by cloud cover asc)
    pub index: HashMap<u64, Vec<usize>>,
    pub quadkey_zoom: u8,
    pub total_scenes: usize,
}

impl MosaicIndex {
    /// Look up scenes covering tile (z, x, y), up to `max_scenes`.
    pub fn scenes_for_tile(
        &self,
        z: u8,
        x: u32,
        y: u32,
        max_scenes: usize,
    ) -> Vec<&SceneRef> {
        let qks = tile_to_covering_quadkeys(z, x, y, self.quadkey_zoom);
        let mut seen = std::collections::HashSet::new();
        let mut result: Vec<&SceneRef> = Vec::new();

        for qk in qks {
            if let Some(indices) = self.index.get(&qk) {
                for &idx in indices {
                    if seen.insert(idx) {
                        result.push(&self.scenes[idx]);
                    }
                }
            }
        }

        // Re-sort by cloud cover (scenes from different quadkeys may interleave)
        result.sort_by(|a, b| {
            a.cloud_cover
                .partial_cmp(&b.cloud_cover)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        result.truncate(max_scenes);
        result
    }

    pub fn scene_count(&self) -> usize {
        self.scenes.len()
    }

    pub fn index_cell_count(&self) -> usize {
        self.index.len()
    }
}

/// Build the mosaic index from STAC items.
///
/// Items should already be sorted by cloud cover ascending.
pub fn build_index(items: &[StacItem], config: &S2Config) -> MosaicIndex {
    let qz = config.quadkey_zoom;
    let mut scenes: Vec<SceneRef> = Vec::new();
    let mut index: HashMap<u64, Vec<usize>> = HashMap::new();

    for item in items {
        let Some(epsg) = item.epsg() else {
            continue;
        };
        let Some(scl_url) = item.scl_url() else {
            continue;
        };
        let band_urls = item.band_urls(&config.bands);
        if band_urls.len() != config.bands.len() {
            continue; // missing one or more bands
        }

        let scene_idx = scenes.len();
        scenes.push(SceneRef {
            id: item.id.clone(),
            band_urls,
            scl_url,
            cloud_cover: item.cloud_cover(),
            datetime: item
                .properties
                .datetime
                .clone()
                .unwrap_or_default(),
            epsg,
        });

        // Determine which quadkeys this scene covers.
        // We use the scene's bbox (from the extent) intersected with the config extent.
        // Since we don't have per-item bbox here easily, use a heuristic:
        // compute quadkeys from the config extent and assign all scenes to all cells.
        // Then at query time, scenes that don't actually cover the tile are filtered by warp.
        //
        // For a production system, we'd compute the scene's actual footprint quadkeys.
        // For now, index all scenes under all quadkeys in the config extent.
        let qks = bbox_to_quadkeys(config.extent, qz);
        for qk in qks {
            index.entry(qk).or_default().push(scene_idx);
        }
    }

    // Each quadkey's scene list is already cloud-cover-sorted (items are pre-sorted)
    let total_scenes = scenes.len();
    info!(
        "Built mosaic index: {} scenes, {} quadkey cells at zoom {}",
        total_scenes,
        index.len(),
        qz
    );

    MosaicIndex {
        scenes,
        index,
        quadkey_zoom: qz,
        total_scenes,
    }
}

/// Serialize index to JSON for fast reload on restart.
pub fn save_index(index: &MosaicIndex, path: &str) -> anyhow::Result<()> {
    let json = serde_json::to_string(index)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Deserialize index from JSON file.
pub fn load_index(path: &str) -> anyhow::Result<MosaicIndex> {
    let json = std::fs::read_to_string(path)?;
    let index: MosaicIndex = serde_json::from_str(&json)?;
    Ok(index)
}
