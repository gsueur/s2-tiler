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
    /// SCL band URL (for cloud masking); None when scl_masking is false
    pub scl_url: Option<String>,
    /// Cloud cover percentage [0, 100]
    pub cloud_cover: f64,
    /// Scene datetime (ISO 8601)
    pub datetime: String,
    /// UTM EPSG code (e.g. 32631 for UTM31N)
    pub epsg: u32,
    /// WGS84 bbox [west, south, east, north] from STAC item
    pub bbox: [f64; 4],
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
    ///
    /// Scenes whose WGS84 bbox does not overlap the tile are filtered out before
    /// truncation — this eliminates false-positive spatial index hits from
    /// adjacent MGRS tiles that share a quadkey cell but don't reach the tile.
    pub fn scenes_for_tile(
        &self,
        z: u8,
        x: u32,
        y: u32,
        max_scenes: usize,
        tile_wgs84: [f64; 4],
    ) -> Vec<&SceneRef> {
        let qks = tile_to_covering_quadkeys(z, x, y, self.quadkey_zoom);
        let mut seen = std::collections::HashSet::new();
        let mut result: Vec<&SceneRef> = Vec::new();

        let [tw, ts, te, tn] = tile_wgs84;

        for qk in qks {
            if let Some(indices) = self.index.get(&qk) {
                for &idx in indices {
                    if seen.insert(idx) {
                        let s = &self.scenes[idx];
                        // Filter: scene bbox must overlap tile bbox
                        let [sw, ss, se, sn] = s.bbox;
                        if se > tw && sw < te && sn > ts && ss < tn {
                            result.push(s);
                        }
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

    /// Look up all unique scenes covering a WGS84 bbox, cloud-cover sorted.
    pub fn scenes_for_bbox(&self, bbox: [f64; 4], max_scenes: usize) -> Vec<&SceneRef> {
        use crate::geo::bbox_to_quadkeys;
        let qks = bbox_to_quadkeys(bbox, self.quadkey_zoom);
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
        let scl_url = if config.scl_masking {
            match item.scl_url() {
                Some(u) => Some(u),
                None => continue, // SCL required but missing — skip scene
            }
        } else {
            None // scl_masking disabled: SCL URL not needed at render time
        };
        let band_urls = item.band_urls(&config.bands);
        if band_urls.len() != config.bands.len() {
            continue; // missing one or more bands
        }

        // Determine quadkeys before pushing to scenes — skip if no overlap.
        // Use the per-item WGS84 bbox from STAC, clipped to the config extent.
        let item_bbox = match item.bbox {
            Some(b) => b,
            None => config.extent, // fallback: no per-item bbox, assume full extent
        };
        // Intersect item bbox with config extent
        let clipped = [
            item_bbox[0].max(config.extent[0]),
            item_bbox[1].max(config.extent[1]),
            item_bbox[2].min(config.extent[2]),
            item_bbox[3].min(config.extent[3]),
        ];
        if clipped[0] >= clipped[2] || clipped[1] >= clipped[3] {
            continue; // item doesn't overlap config extent at all
        }
        let qks = bbox_to_quadkeys(clipped, qz);
        if qks.is_empty() {
            continue;
        }

        let scene_idx = scenes.len();
        scenes.push(SceneRef {
            id: item.id.clone(),
            band_urls,
            scl_url,
            cloud_cover: item.cloud_cover(),
            datetime: item.properties.datetime.clone().unwrap_or_default(),
            epsg,
            bbox: item_bbox,
        });
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

/// Ensure the shared index database schema exists.
fn init_schema(conn: &duckdb::Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
             tileset      TEXT    NOT NULL PRIMARY KEY,
             quadkey_zoom INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS scenes (
             tileset  TEXT    NOT NULL,
             idx      INTEGER NOT NULL,
             id       TEXT    NOT NULL,
             cloud    DOUBLE  NOT NULL,
             dt       TEXT    NOT NULL,
             epsg     INTEGER NOT NULL,
             scl_url  TEXT    NOT NULL,
             bands    TEXT    NOT NULL,
             bbox_w   DOUBLE  NOT NULL DEFAULT 0,
             bbox_s   DOUBLE  NOT NULL DEFAULT 0,
             bbox_e   DOUBLE  NOT NULL DEFAULT 0,
             bbox_n   DOUBLE  NOT NULL DEFAULT 0,
             PRIMARY KEY (tileset, idx)
         );
         CREATE TABLE IF NOT EXISTS scene_quadkeys (
             tileset   TEXT    NOT NULL,
             quadkey   BIGINT  NOT NULL,
             scene_idx INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS qk_idx ON scene_quadkeys(tileset, quadkey);",
    )?;
    Ok(())
}

/// Persist a tileset's mosaic index into the shared DuckDB database at `path`.
///
/// Creates the file and schema on first use. Replaces existing rows for `tileset`.
pub fn save_index(index: &MosaicIndex, tileset: &str, path: &str) -> anyhow::Result<()> {
    let conn = duckdb::Connection::open(path)?;
    init_schema(&conn)?;

    // Replace existing data for this tileset.
    conn.execute("DELETE FROM scene_quadkeys WHERE tileset = ?", duckdb::params![tileset])?;
    conn.execute("DELETE FROM scenes WHERE tileset = ?", duckdb::params![tileset])?;
    conn.execute("DELETE FROM meta WHERE tileset = ?", duckdb::params![tileset])?;

    conn.execute(
        "INSERT INTO meta VALUES (?, ?)",
        duckdb::params![tileset, index.quadkey_zoom as i32],
    )?;

    {
        let mut app = conn.appender("scenes")?;
        for (idx, s) in index.scenes.iter().enumerate() {
            let bands = serde_json::to_string(&s.band_urls)?;
            app.append_row(duckdb::params![
                tileset,
                idx as i32,
                s.id.as_str(),
                s.cloud_cover,
                s.datetime.as_str(),
                s.epsg as i32,
                s.scl_url.as_deref().unwrap_or(""),
                bands.as_str(),
                s.bbox[0],
                s.bbox[1],
                s.bbox[2],
                s.bbox[3]
            ])?;
        }
        app.flush()?;
    }

    {
        let mut app = conn.appender("scene_quadkeys")?;
        for (qk, indices) in &index.index {
            for &scene_idx in indices {
                app.append_row(duckdb::params![tileset, *qk as i64, scene_idx as i32])?;
            }
        }
        app.flush()?;
    }

    Ok(())
}

/// Load a tileset's mosaic index from the shared DuckDB database at `path`.
pub fn load_index(tileset: &str, path: &str) -> anyhow::Result<MosaicIndex> {
    let conn = duckdb::Connection::open(path)?;

    let quadkey_zoom: u8 = {
        let v: i32 = conn.query_row(
            "SELECT quadkey_zoom FROM meta WHERE tileset = ?",
            duckdb::params![tileset],
            |row| row.get(0),
        )?;
        v as u8
    };

    let mut stmt = conn.prepare(
        "SELECT idx, id, cloud, dt, epsg, scl_url, bands,
                bbox_w, bbox_s, bbox_e, bbox_n
         FROM scenes WHERE tileset = ? ORDER BY idx",
    )?;
    let mut scenes: Vec<SceneRef> = Vec::new();
    let rows = stmt.query_map(duckdb::params![tileset], |row| {
        Ok((
            row.get::<_, i32>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i32>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, f64>(7)?,
            row.get::<_, f64>(8)?,
            row.get::<_, f64>(9)?,
            row.get::<_, f64>(10)?,
        ))
    })?;
    for row in rows {
        let (_, id, cloud_cover, datetime, epsg, scl_url_str, bands_json,
             bbox_w, bbox_s, bbox_e, bbox_n) = row?;
        let band_urls = serde_json::from_str(&bands_json)?;
        let scl_url: Option<String> = if scl_url_str.is_empty() { None } else { Some(scl_url_str) };
        scenes.push(SceneRef {
            id,
            cloud_cover,
            datetime,
            epsg: epsg as u32,
            scl_url,
            band_urls,
            bbox: [bbox_w, bbox_s, bbox_e, bbox_n],
        });
    }

    let mut stmt = conn.prepare(
        "SELECT quadkey, scene_idx FROM scene_quadkeys WHERE tileset = ?",
    )?;
    let mut index: HashMap<u64, Vec<usize>> = HashMap::new();
    let rows = stmt.query_map(duckdb::params![tileset], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i32>(1)?))
    })?;
    for row in rows {
        let (qk, scene_idx) = row?;
        index
            .entry(qk as u64)
            .or_default()
            .push(scene_idx as usize);
    }

    let total_scenes = scenes.len();
    Ok(MosaicIndex {
        scenes,
        index,
        quadkey_zoom,
        total_scenes,
    })
}
