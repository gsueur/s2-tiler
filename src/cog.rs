/// Async COG reader: opens remote GeoTIFF files, caches IFD metadata,
/// and reads windows into ndarray, applying overview selection.
use crate::geo::{Affine, Bbox};
use anyhow::{Context, Result};
use async_tiff::{
    decoder::DecoderRegistry,
    metadata::{cache::ReadaheadMetadataCache, TiffMetadataReader},
    reader::{AsyncFileReader, ObjectReader},
    ImageFileDirectory, TIFF,
};
use dashmap::DashMap;
use ndarray::{Array2, Array3};
use object_store::{http::HttpBuilder, path::Path as ObjPath};
use std::sync::Arc;
use tracing::debug;
use url::Url;

// ─── Cached entry per COG URL ───────────────────────────────────────────────

struct CachedCog {
    tiff: TIFF,
    reader: ObjectReader,
}

// ─── Public reader ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct CogReader {
    /// IFD metadata cache: URL → parsed TIFF + reader
    cache: Arc<DashMap<String, Arc<CachedCog>>>,
    /// HTTP store cache: hostname → shared ObjectStore (reuses TCP connections)
    stores: Arc<DashMap<String, Arc<dyn object_store::ObjectStore>>>,
    decoder: Arc<DecoderRegistry>,
}

impl CogReader {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(DashMap::new()),
            stores: Arc::new(DashMap::new()),
            decoder: Arc::new(DecoderRegistry::default()),
        }
    }

    /// Return (or create) a shared ObjectStore for the given hostname base URL.
    fn get_or_create_store(&self, base: &str) -> Result<Arc<dyn object_store::ObjectStore>> {
        if let Some(store) = self.stores.get(base) {
            return Ok(Arc::clone(&*store));
        }
        let store: Arc<dyn object_store::ObjectStore> = Arc::new(
            HttpBuilder::new()
                .with_url(base)
                .build()
                .context("creating HTTP object store")?,
        );
        self.stores.insert(base.to_string(), Arc::clone(&store));
        Ok(store)
    }

    /// Open a COG URL, parsing IFDs and caching the result.
    async fn open(&self, url: &str) -> Result<Arc<CachedCog>> {
        if let Some(entry) = self.cache.get(url) {
            return Ok(Arc::clone(&*entry));
        }

        debug!("Opening COG: {url}");

        let parsed = Url::parse(url).context("invalid COG URL")?;
        let base = format!(
            "{}://{}",
            parsed.scheme(),
            parsed.host_str().unwrap_or("localhost")
        );
        let obj_path = ObjPath::from(parsed.path().trim_start_matches('/'));

        let store = self.get_or_create_store(&base)?;
        let reader = ObjectReader::new(store, obj_path);

        // Wrap in ReadaheadMetadataCache for efficient IFD parsing
        let cached = ReadaheadMetadataCache::new(reader.clone());
        let mut meta = TiffMetadataReader::try_open(&cached)
            .await
            .context("reading TIFF header")?;
        let tiff = meta.read(&cached).await.context("reading TIFF IFDs")?;

        let entry = Arc::new(CachedCog { tiff, reader });
        self.cache.insert(url.to_string(), Arc::clone(&entry));
        Ok(entry)
    }

    /// Read a spatial window from a single-band u16 COG (e.g., B04).
    ///
    /// * `url`         — HTTPS URL to the COG
    /// * `window_utm`  — bbox in the COG's native UTM CRS (meters)
    /// * `desired_gsd` — target GSD (m/px); used to pick an overview level
    ///
    /// Returns `(data, affine)` where data is 2D and affine is the geo-transform
    /// in the native UTM CRS.
    pub async fn read_window_u16(
        &self,
        url: &str,
        window_utm: &Bbox,
        desired_gsd: f64,
    ) -> Result<(Array2<u16>, Affine)> {
        let cog = self.open(url).await?;
        let ifd = select_overview(&cog.tiff, desired_gsd)?;
        let affine = ifd_to_affine_with_fallback(ifd, cog.tiff.ifds())?;
        let window_affine = window_origin_affine(&affine, window_utm, &ifd);

        let data = read_ifd_window(ifd, &cog.reader, &affine, window_utm, &self.decoder).await?;

        let h = data.shape()[1];
        let w = data.shape()[2];
        let flat = data.into_raw_vec();
        Ok((
            Array2::from_shape_vec((h, w), flat).context("reshaping band array")?,
            window_affine,
        ))
    }

    /// Read a spatial window from a single-band u8 COG (SCL).
    pub async fn read_window_u8(
        &self,
        url: &str,
        window_utm: &Bbox,
        desired_gsd: f64,
    ) -> Result<(Array2<u8>, Affine)> {
        let cog = self.open(url).await?;
        let ifd = select_overview(&cog.tiff, desired_gsd)?;
        let affine = ifd_to_affine_with_fallback(ifd, cog.tiff.ifds())?;
        let window_affine = window_origin_affine(&affine, window_utm, &ifd);

        let data = read_ifd_window(ifd, &cog.reader, &affine, window_utm, &self.decoder).await?;

        let h = data.shape()[1];
        let w = data.shape()[2];
        // SCL values are 0-11: safe to cast u16 → u8
        let flat: Vec<u8> = data.into_raw_vec().into_iter().map(|v| v as u8).collect();
        Ok((
            Array2::from_shape_vec((h, w), flat).context("reshaping SCL array")?,
            window_affine,
        ))
    }
}

// ─── Window-origin affine ────────────────────────────────────────────────────

/// Compute an affine whose origin is the top-left corner of the requested
/// window (not the full granule). The returned affine has the same pixel size
/// as the granule affine but its origin_x/origin_y reflect the clamped
/// col_min/row_min, so pixel index (0,0) corresponds to the window's NW corner.
fn window_origin_affine(affine: &Affine, window_utm: &Bbox, ifd: &ImageFileDirectory) -> Affine {
    let img_w = ifd.image_width() as f64;
    let img_h = ifd.image_height() as f64;
    let col_min = ((window_utm.x_min - affine.origin_x) / affine.pixel_width)
        .floor()
        .max(0.0)
        .min(img_w);
    let row_min = ((affine.origin_y - window_utm.y_max) / affine.pixel_height)
        .floor()
        .max(0.0)
        .min(img_h);
    Affine {
        origin_x: affine.origin_x + col_min * affine.pixel_width,
        origin_y: affine.origin_y - row_min * affine.pixel_height,
        pixel_width: affine.pixel_width,
        pixel_height: affine.pixel_height,
    }
}

// ─── IFD helpers ────────────────────────────────────────────────────────────

fn select_overview<'a>(tiff: &'a TIFF, desired_gsd: f64) -> Result<&'a ImageFileDirectory> {
    let ifds = tiff.ifds();
    anyhow::ensure!(!ifds.is_empty(), "TIFF has no IFDs");

    let full_w = ifds[0].image_width() as f64;
    let full_gsd = ifd_pixel_size(&ifds[0])?;
    let mut best = &ifds[0];
    let mut best_gsd = full_gsd;

    for ifd in &ifds[1..] {
        // Try ModelPixelScale first; fall back to image-width ratio.
        let gsd = if let Ok(g) = ifd_pixel_size(ifd) {
            g
        } else {
            // Overview IFDs often lack ModelPixelScale; infer GSD from width ratio.
            let ovr_w = ifd.image_width() as f64;
            if ovr_w <= 0.0 {
                break;
            }
            full_gsd * (full_w / ovr_w)
        };
        // Allow up to 2× upsampling: select the coarsest overview within that range.
        // This reduces HTTP fetch volume at low zoom levels without visible quality loss.
        if gsd <= desired_gsd * 2.0 {
            best = ifd;
            best_gsd = gsd;
        } else {
            break;
        }
    }

    debug!("Selected overview GSD={best_gsd:.1}m (desired {desired_gsd:.1}m)");
    Ok(best)
}

fn ifd_pixel_size(ifd: &ImageFileDirectory) -> Result<f64> {
    Ok(ifd
        .model_pixel_scale()
        .context("IFD missing ModelPixelScale")?[0]
        .abs())
}

pub fn ifd_to_affine(ifd: &ImageFileDirectory) -> Result<Affine> {
    let scale = ifd
        .model_pixel_scale()
        .context("IFD missing ModelPixelScale")?;
    let tp = ifd
        .model_tiepoint()
        .context("IFD missing ModelTiepoint")?;
    Ok(Affine {
        origin_x: tp[3],
        origin_y: tp[4],
        pixel_width: scale[0].abs(),
        pixel_height: scale[1].abs(),
    })
}

/// Like `ifd_to_affine` but falls back to the full-res (IFD[0]) tiepoint when
/// the selected overview IFD lacks ModelTiepoint (common for overview IFDs).
/// The overview pixel scale is inferred from the image-width ratio if ModelPixelScale is absent.
fn ifd_to_affine_with_fallback(
    ifd: &ImageFileDirectory,
    all_ifds: &[ImageFileDirectory],
) -> Result<Affine> {
    // Prefer the IFD's own tiepoint + scale when available
    if ifd.model_pixel_scale().is_some() && ifd.model_tiepoint().is_some() {
        return ifd_to_affine(ifd);
    }

    // Fall back to full-res IFD[0] for tiepoint and origin
    let full_ifd = all_ifds.first().context("TIFF has no IFDs")?;
    let full_affine = ifd_to_affine(full_ifd)?;

    // Infer pixel scale from image-width ratio
    let full_w = full_ifd.image_width() as f64;
    let ovr_w = ifd.image_width() as f64;
    let scale_factor = if ovr_w > 0.0 { full_w / ovr_w } else { 1.0 };

    Ok(Affine {
        origin_x: full_affine.origin_x,
        origin_y: full_affine.origin_y,
        pixel_width: full_affine.pixel_width * scale_factor,
        pixel_height: full_affine.pixel_height * scale_factor,
    })
}

// ─── Window reading ─────────────────────────────────────────────────────────

/// Read all internal COG tiles covering `window_utm`, assemble into Array3<u16>
/// with shape (1, height, width).
async fn read_ifd_window(
    ifd: &ImageFileDirectory,
    reader: &(impl AsyncFileReader + Sync),
    affine: &Affine,
    window_utm: &Bbox,
    decoder: &DecoderRegistry,
) -> Result<Array3<u16>> {
    let tile_w = ifd
        .tile_width()
        .context("COG not tiled (missing TileWidth)")? as usize;
    let tile_h = ifd
        .tile_height()
        .context("COG not tiled (missing TileHeight)")? as usize;
    let (n_tx, n_ty) = ifd.tile_count().context("COG missing tile count")?;
    let img_w = ifd.image_width() as usize;
    let img_h = ifd.image_height() as usize;

    // Convert window bbox → pixel range in this overview, clamped to image bounds
    let col_min = ((window_utm.x_min - affine.origin_x) / affine.pixel_width)
        .floor()
        .max(0.0) as usize;
    let row_min = ((affine.origin_y - window_utm.y_max) / affine.pixel_height)
        .floor()
        .max(0.0) as usize;
    let col_max = ((window_utm.x_max - affine.origin_x) / affine.pixel_width)
        .ceil()
        .min(img_w as f64) as usize;
    let row_max = ((affine.origin_y - window_utm.y_min) / affine.pixel_height)
        .ceil()
        .min(img_h as f64) as usize;

    if col_min >= col_max || row_min >= row_max {
        // Window doesn't intersect the raster
        return Ok(Array3::zeros((1, 1, 1)));
    }

    let tx_min = col_min / tile_w;
    let ty_min = row_min / tile_h;
    let tx_max = (col_max.saturating_sub(1) / tile_w).min(n_tx - 1);
    let ty_max = (row_max.saturating_sub(1) / tile_h).min(n_ty - 1);

    let mut xs: Vec<usize> = Vec::new();
    let mut ys: Vec<usize> = Vec::new();
    for ty in ty_min..=ty_max {
        for tx in tx_min..=tx_max {
            xs.push(tx);
            ys.push(ty);
        }
    }

    debug!(
        "Fetching {} tiles for window ({col_min}..{col_max}, {row_min}..{row_max})",
        xs.len()
    );

    let tiles = ifd
        .fetch_tiles(&xs, &ys, reader)
        .await
        .context("fetching COG tiles")?;

    let out_w = col_max - col_min;
    let out_h = row_max - row_min;
    let mut output = Array3::<u16>::zeros((1, out_h, out_w));

    for tile in tiles {
        let tx = tile.x();
        let ty = tile.y();

        let array = tile.decode(decoder).context("decoding tile")?;
        let shape = array.shape(); // [usize; 3]

        // For chunky (h, w, bands): shape = [h, w, 1]
        // For planar (bands, h, w): shape = [1, h, w]
        // S2 single-band COGs are typically chunky.
        // Detect by whether the last or first dimension is 1.
        let (t_h, t_w) = if shape[2] <= shape[0] {
            // Chunky: (h, w, bands) — bands is last (smallest for single-band)
            (shape[0], shape[1])
        } else {
            // Planar: (bands, h, w) — bands is first
            (shape[1], shape[2])
        };

        let pixel_bytes = array.data_type().map(|dt| dt.size()).unwrap_or(2);
        // Get raw bytes via TypedArray's AsRef<[u8]>
        let raw_bytes: &[u8] = array.data().as_ref();

        let tc_start = tx * tile_w;
        let tr_start = ty * tile_h;

        let overlap_c0 = tc_start.max(col_min);
        let overlap_c1 = (tc_start + t_w).min(col_max);
        let overlap_r0 = tr_start.max(row_min);
        let overlap_r1 = (tr_start + t_h).min(row_max);

        if overlap_c0 >= overlap_c1 || overlap_r0 >= overlap_r1 {
            continue;
        }

        let src_c0 = overlap_c0 - tc_start;
        let src_r0 = overlap_r0 - tr_start;
        let dst_c0 = overlap_c0 - col_min;
        let dst_r0 = overlap_r0 - row_min;
        let copy_w = overlap_c1 - overlap_c0;
        let copy_h = overlap_r1 - overlap_r0;

        for r in 0..copy_h {
            for c in 0..copy_w {
                let src_idx = (src_r0 + r) * t_w + (src_c0 + c);
                let byte_off = src_idx * pixel_bytes;
                let val = if pixel_bytes >= 2 && byte_off + 1 < raw_bytes.len() {
                    u16::from_le_bytes([raw_bytes[byte_off], raw_bytes[byte_off + 1]])
                } else if byte_off < raw_bytes.len() {
                    raw_bytes[byte_off] as u16
                } else {
                    0
                };
                output[[0, dst_r0 + r, dst_c0 + c]] = val;
            }
        }
    }

    Ok(output)
}
