use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Composite {
    BestPixel,
    Median,
    Latest,
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
        anyhow::ensure!(
            self.bands.len() == 3 || self.bands.len() == 1,
            "bands must have 1 (grayscale) or 3 (RGB) entries"
        );
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

    /// Return RFC3339 datetime range strings for STAC search (one per year × season).
    pub fn datetime_ranges(&self) -> Vec<String> {
        let mut ranges = Vec::new();
        for &year in &self.years {
            if let Some(months) = &self.season {
                if !months.is_empty() {
                    let min_m = months.iter().copied().min().unwrap();
                    let max_m = months.iter().copied().max().unwrap();
                    let end_day = days_in_month(year, max_m);
                    ranges.push(format!(
                        "{year}-{min_m:02}-01T00:00:00Z/{year}-{max_m:02}-{end_day:02}T23:59:59Z"
                    ));
                }
            } else {
                ranges.push(format!("{year}-01-01T00:00:00Z/{year}-12-31T23:59:59Z"));
            }
        }
        ranges
    }
}
