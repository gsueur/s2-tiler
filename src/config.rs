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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S2Config {
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

    /// Server bind port
    #[serde(default = "default_port")]
    pub port: u16,

    /// Max scenes to composite per tile (limits cold tile latency)
    #[serde(default = "default_max_scenes_per_tile")]
    pub max_scenes_per_tile: usize,
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
fn default_port() -> u16 {
    3000
}
fn default_max_scenes_per_tile() -> usize {
    6
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

impl S2Config {
    pub fn from_yaml_file(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: S2Config = serde_yaml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.years.is_empty(), "years must not be empty");
        anyhow::ensure!(!self.bands.is_empty(), "bands must not be empty");
        anyhow::ensure!(
            self.bands.len() == 3 || self.bands.len() == 1,
            "bands must have 1 (grayscale) or 3 (RGB) entries"
        );
        anyhow::ensure!(
            self.minzoom <= self.maxzoom,
            "minzoom must be <= maxzoom"
        );
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

    /// Return datetime range strings for STAC search (one per year × season combination)
    pub fn datetime_ranges(&self) -> Vec<String> {
        let mut ranges = Vec::new();
        for &year in &self.years {
            if let Some(months) = &self.season {
                // Group consecutive months into ranges
                if !months.is_empty() {
                    let min_m = months.iter().copied().min().unwrap();
                    let max_m = months.iter().copied().max().unwrap();
                    ranges.push(format!("{year}-{min_m:02}-01/{year}-{max_m:02}-28"));
                }
            } else {
                ranges.push(format!("{year}-01-01/{year}-12-31"));
            }
        }
        ranges
    }
}
