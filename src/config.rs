use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Composite {
    BestPixel,
    Median,
    Latest,
    /// NDVI = (NIR - Red) / (NIR + Red). Requires exactly 2 bands: [NIR, Red] (e.g. [B08, B04]).
    /// Rescale values are interpreted as NDVI float range, e.g. rescale: [-1, 1] or [0, 1].
    Ndvi,
}

impl Default for Composite {
    fn default() -> Self {
        Composite::BestPixel
    }
}

/// Per-tileset configuration. Fields shared across all tilesets (minzoom, maxzoom,
/// quadkey_zoom, stac_url, collection) are defined once at the AppConfig level and
/// injected here after parsing — do not set them inside a tilesets entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S2Config {
    /// Unique tileset identifier; used as the URL prefix (e.g. /massachusetts/{z}/{x}/{y})
    pub name: String,

    /// WGS84 bounding box [minx, miny, maxx, maxy]
    pub extent: [f64; 4],

    /// Years to include (e.g. [2023, 2024])
    pub years: Vec<u32>,

    /// Month numbers for seasonal filter (e.g. [6, 7, 8] for JJA); omit for full year
    #[serde(default)]
    pub season: Option<Vec<u8>>,

    /// Maximum eo:cloud_cover for STAC pre-filter
    #[serde(default = "default_max_cloud_cover")]
    pub max_cloud_cover: f64,

    /// S2 band codes to render (e.g. ["B04", "B03", "B02"] for RGB)
    pub bands: Vec<String>,

    /// Compositing strategy
    #[serde(default)]
    pub composite: Composite,

    /// S2 L2A SR value range mapped to [0, 255] for display
    #[serde(default = "default_rescale")]
    pub rescale: [f64; 2],

    /// Max scenes to composite per tile (limits cold tile latency)
    #[serde(default = "default_max_scenes_per_tile")]
    pub max_scenes_per_tile: usize,

    /// Haze rejection threshold: pixels where ALL bands exceed this DN value are
    /// considered haze/thin-cloud and excluded from the composite (0 = disabled).
    /// For true-color rescale [0, 3000]: ~2400. For NIR [0, 4000]: ~3200.
    /// Ignored when scl_masking is false.
    #[serde(default)]
    pub haze_dn_max: u16,

    /// When true (default), pixels are filtered by SCL class (only classes 4/5/6/7
    /// are considered valid) and haze_dn_max. When false, any pixel within the scene
    /// footprint is used as-is — scenes are composited whole, sorted by cloud cover.
    /// Avoids false masking of bright urban surfaces, sand, or snow. SCL is not read
    /// from S3 when this is false, saving one HTTP request per scene per tile.
    #[serde(default = "default_scl_masking")]
    pub scl_masking: bool,

    /// When true, scenes are sorted by most-recent year/month first, then cloud cover
    /// within each period. Produces more temporally homogeneous composites — adjacent
    /// tiles draw from the same year/month before falling back to older scenes.
    /// Default false (sort by cloud cover only).
    #[serde(default)]
    pub temporal_priority: bool,

    // ── Injected from AppConfig after parsing ────────────────────────────────

    #[serde(skip_deserializing)]
    pub minzoom: u8,

    #[serde(skip_deserializing)]
    pub maxzoom: u8,

    #[serde(skip_deserializing)]
    pub quadkey_zoom: u8,

    #[serde(skip_deserializing)]
    pub stac_url: String,

    #[serde(skip_deserializing)]
    pub collection: String,
}

/// Tile cache backend configuration.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum TileCacheConfig {
    /// No caching — tiles are rendered on every request.
    None,
    /// In-process DashMap (default). Fast; lost on restart.
    #[default]
    Memory,
    /// Local filesystem. `path` is the root directory.
    Local { path: String },
    /// DuckDB table in the file at `path`.
    Duckdb { path: String },
    /// Amazon S3. Credentials from AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY env vars.
    S3 {
        bucket: String,
        #[serde(default = "default_region")]
        region: String,
        #[serde(default)]
        prefix: Option<String>,
    },
    /// Cloudflare R2 (S3-compatible). Credentials from R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY.
    R2 {
        bucket: String,
        account_id: String,
        #[serde(default)]
        prefix: Option<String>,
    },
}

fn default_region() -> String {
    "us-east-1".to_string()
}

/// Top-level application config: global settings + one or more named tilesets.
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    /// Server bind port
    #[serde(default = "default_port")]
    pub port: u16,

    /// Minimum zoom level served (below this → 404)
    #[serde(default = "default_minzoom")]
    pub minzoom: u8,

    /// Maximum zoom level served (above this → 404)
    #[serde(default = "default_maxzoom")]
    pub maxzoom: u8,

    /// Quadkey zoom for MosaicJSON spatial index
    #[serde(default = "default_quadkey_zoom")]
    pub quadkey_zoom: u8,

    /// STAC API base URL
    #[serde(default = "default_stac_url")]
    pub stac_url: String,

    /// STAC collection ID
    #[serde(default = "default_collection")]
    pub collection: String,

    /// Path to the shared DuckDB index database; all tilesets stored in one file
    #[serde(default)]
    pub index_path: Option<String>,

    /// Tile cache backend (default: memory)
    #[serde(default)]
    pub tile_cache: TileCacheConfig,

    /// Public base URL for TileJSON tile URLs (e.g. "https://tiles.example.com").
    /// Defaults to "http://localhost:{port}" if not set.
    #[serde(default)]
    pub public_url: Option<String>,

    /// Cache-Control max-age in seconds for tile responses.
    /// Omit or set to None to suppress the Cache-Control header.
    #[serde(default)]
    pub cache_max_age: Option<u64>,

    pub tilesets: Vec<S2Config>,
}

impl AppConfig {
    pub fn from_yaml_file(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: AppConfig = serde_yaml::from_str(&content)?;
        anyhow::ensure!(!config.tilesets.is_empty(), "at least one tileset must be defined");
        for ts in &mut config.tilesets {
            ts.minzoom = config.minzoom;
            ts.maxzoom = config.maxzoom;
            ts.quadkey_zoom = config.quadkey_zoom;
            ts.stac_url = config.stac_url.clone();
            ts.collection = config.collection.clone();
            ts.validate()?;
        }
        Ok(config)
    }
}

fn default_max_cloud_cover() -> f64 {
    20.0
}
fn default_rescale() -> [f64; 2] {
    [0.0, 3000.0]
}
fn default_minzoom() -> u8 {
    10
}
fn default_maxzoom() -> u8 {
    15
}
fn default_quadkey_zoom() -> u8 {
    8
}
fn default_stac_url() -> String {
    "https://earth-search.aws.element84.com/v1".to_string()
}
fn default_collection() -> String {
    "sentinel-2-l2a".to_string()
}
fn default_max_scenes_per_tile() -> usize {
    6
}
fn default_port() -> u16 {
    3000
}
fn default_scl_masking() -> bool {
    true
}

/// S2 band code → Earth Search v1 asset key
pub fn band_to_asset(band: &str) -> Option<&'static str> {
    match band {
        "B01" => Some("coastal"),
        "B02" => Some("blue"),
        "B03" => Some("green"),
        "B04" => Some("red"),
        "B05" => Some("rededge1"),
        "B06" => Some("rededge2"),
        "B07" => Some("rededge3"),
        "B08" => Some("nir"),
        "B8A" => Some("nir08"),
        "B09" => Some("nir09"),
        "B11" => Some("swir16"),
        "B12" => Some("swir22"),
        "SCL" => Some("scl"),
        _ => None,
    }
}

fn days_in_month(year: u32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

impl S2Config {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.name.is_empty(), "tileset name must not be empty");
        anyhow::ensure!(!self.years.is_empty(), "years must not be empty");
        anyhow::ensure!(!self.bands.is_empty(), "bands must not be empty");
        match &self.composite {
            Composite::Ndvi => anyhow::ensure!(
                self.bands.len() == 2,
                "composite: ndvi requires exactly 2 bands: [NIR, Red] (e.g. [B08, B04])"
            ),
            _ => anyhow::ensure!(
                self.bands.len() == 1 || self.bands.len() == 3,
                "bands must have 1 (grayscale) or 3 (RGB) entries"
            ),
        }
        anyhow::ensure!(self.minzoom <= self.maxzoom, "minzoom must be <= maxzoom");
        anyhow::ensure!(
            self.extent[0] < self.extent[2] && self.extent[1] < self.extent[3],
            "extent must be [west, south, east, north]"
        );
        for band in &self.bands {
            anyhow::ensure!(
                band_to_asset(band).is_some(),
                "unknown band: {band}; supported: B01-B12, B8A, SCL"
            );
        }
        Ok(())
    }

    /// Return RFC3339 datetime range strings for STAC search (one per year × month).
    ///
    /// Produces one range per (year, month) pair rather than a single span from
    /// min_month to max_month, so non-contiguous seasons like [3, 6, 9] are not
    /// incorrectly expanded to include intermediate months.
    pub fn datetime_ranges(&self) -> Vec<String> {
        let mut ranges = Vec::new();
        for &year in &self.years {
            if let Some(months) = &self.season {
                if !months.is_empty() {
                    for &month in months {
                        let end_day = days_in_month(year, month);
                        ranges.push(format!(
                            "{year}-{month:02}-01T00:00:00Z/{year}-{month:02}-{end_day:02}T23:59:59Z"
                        ));
                    }
                }
            } else {
                ranges.push(format!("{year}-01-01T00:00:00Z/{year}-12-31T23:59:59Z"));
            }
        }
        ranges
    }
}
