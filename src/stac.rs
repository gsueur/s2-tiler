use crate::config::{S2Config, band_to_asset};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StacAsset {
    pub href: String,
    #[serde(rename = "type")]
    pub media_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StacItemProperties {
    #[serde(rename = "eo:cloud_cover")]
    pub cloud_cover: Option<f64>,
    pub datetime: Option<String>,
    #[serde(rename = "proj:epsg")]
    pub proj_epsg: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StacItem {
    pub id: String,
    /// WGS84 [minx, miny, maxx, maxy]
    pub bbox: Option<[f64; 4]>,
    pub properties: StacItemProperties,
    pub assets: HashMap<String, StacAsset>,
}

impl StacItem {
    pub fn cloud_cover(&self) -> f64 {
        self.properties.cloud_cover.unwrap_or(100.0)
    }

    pub fn epsg(&self) -> Option<u32> {
        self.properties.proj_epsg
    }

    pub fn asset_href(&self, asset_key: &str) -> Option<String> {
        self.assets.get(asset_key).map(|a| a.href.clone())
    }

    /// Returns map of band_code → HTTPS URL for all requested bands + SCL
    pub fn band_urls(&self, bands: &[String]) -> HashMap<String, String> {
        let mut urls = HashMap::new();
        for band in bands {
            if let Some(asset_key) = band_to_asset(band) {
                if let Some(href) = self.asset_href(asset_key) {
                    urls.insert(band.clone(), href);
                }
            }
        }
        urls
    }

    pub fn scl_url(&self) -> Option<String> {
        self.asset_href("scl")
    }
}

#[derive(Debug, Deserialize)]
struct StacSearchResponse {
    features: Vec<StacItem>,
    links: Option<Vec<StacLink>>,
}

#[derive(Debug, Deserialize)]
struct StacLink {
    rel: String,
    href: Option<String>,
    method: Option<String>,
    body: Option<serde_json::Value>,
}

/// Search STAC API and return all matching items sorted by cloud cover ascending.
pub async fn search_items(config: &S2Config, client: &reqwest::Client) -> Result<Vec<StacItem>> {
    let search_url = format!("{}/search", config.stac_url.trim_end_matches('/'));
    let mut all_items: Vec<StacItem> = Vec::new();

    for datetime_range in config.datetime_ranges() {
        info!(
            "Searching STAC: collection={}, datetime={}, bbox={:?}, max_cloud={}",
            config.collection, datetime_range, config.extent, config.max_cloud_cover
        );

        let items = search_paginated(config, client, &search_url, &datetime_range).await?;
        info!("  → {} items for {datetime_range}", items.len());
        all_items.extend(items);
    }

    // Deduplicate by ID (same scene can appear in multiple datetime ranges)
    all_items.sort_by(|a, b| a.id.cmp(&b.id));
    all_items.dedup_by_key(|i| i.id.clone());

    // Filter out items missing required assets
    let required_assets: Vec<&str> = config
        .bands
        .iter()
        .filter_map(|b| band_to_asset(b))
        .collect();

    all_items.retain(|item| {
        let ok = required_assets.iter().all(|asset| item.assets.contains_key(*asset))
            && item.scl_url().is_some()
            && item.epsg().is_some();
        if !ok {
            warn!("Skipping item {} (missing assets or proj:epsg)", item.id);
        }
        ok
    });

    // Sort by cloud cover ascending (best scenes first for best_pixel composite)
    all_items.sort_by(|a, b| {
        a.cloud_cover()
            .partial_cmp(&b.cloud_cover())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    info!("Total items after filtering + dedup: {}", all_items.len());
    Ok(all_items)
}

async fn search_paginated(
    config: &S2Config,
    client: &reqwest::Client,
    search_url: &str,
    datetime_range: &str,
) -> Result<Vec<StacItem>> {
    let mut items = Vec::new();
    let mut page = 0usize;

    let [west, south, east, north] = config.extent;

    let mut body = serde_json::json!({
        "collections": [config.collection],
        "bbox": [west, south, east, north],
        "datetime": datetime_range,
        "query": {
            "eo:cloud_cover": {"lt": config.max_cloud_cover}
        },
        "limit": 200
    });

    let mut url = search_url.to_string();

    loop {
        debug!("STAC search page {page}: POST {url}");
        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("STAC search request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("STAC search returned {status}: {text}");
        }

        let result: StacSearchResponse = resp.json().await.context("parsing STAC response")?;
        let n = result.features.len();
        items.extend(result.features);
        page += 1;

        // Follow next-page link if present
        let next_link = result
            .links
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .find(|l| l.rel == "next");

        match next_link {
            Some(link) if n > 0 => {
                // Some STAC APIs use POST with a body for next-page
                if link.method.as_deref() == Some("POST") {
                    if let Some(next_body) = &link.body {
                        body = next_body.clone();
                    }
                    if let Some(href) = &link.href {
                        url = href.clone();
                    }
                } else if let Some(href) = &link.href {
                    // GET-based pagination (less common for search)
                    url = href.clone();
                    body = serde_json::json!({});
                } else {
                    break;
                }
            }
            _ => break,
        }

        // Safety limit: avoid infinite pagination
        if page > 50 {
            warn!("Stopped pagination at page {page}");
            break;
        }
    }

    Ok(items)
}
