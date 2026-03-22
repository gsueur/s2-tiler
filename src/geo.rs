/// Tile / CRS geometry utilities.
/// All coordinates in WebMercator are in meters; WGS84 in degrees.
use anyhow::Result;

// Earth radius used by WebMercator (EPSG:3857)
const WM_R: f64 = 6378137.0;
const WM_HALF_SIZE: f64 = std::f64::consts::PI * WM_R; // ~20037508 m

// ─── Tile math ─────────────────────────────────────────────────────────────

/// Axis-aligned bounding box
#[derive(Debug, Clone, Copy)]
pub struct Bbox {
    pub x_min: f64,
    pub y_min: f64,
    pub x_max: f64,
    pub y_max: f64,
}

impl Bbox {
    pub fn width(&self) -> f64 {
        self.x_max - self.x_min
    }
    pub fn height(&self) -> f64 {
        self.y_max - self.y_min
    }
}

/// Convert XYZ tile to WebMercator bbox (meters).
pub fn xyz_to_webmercator(z: u8, x: u32, y: u32) -> Bbox {
    let n = (1u32 << z) as f64;
    let tile_size = 2.0 * WM_HALF_SIZE / n;
    Bbox {
        x_min: x as f64 * tile_size - WM_HALF_SIZE,
        y_min: WM_HALF_SIZE - (y as f64 + 1.0) * tile_size,
        x_max: (x as f64 + 1.0) * tile_size - WM_HALF_SIZE,
        y_max: WM_HALF_SIZE - y as f64 * tile_size,
    }
}

/// WebMercator (meters) → WGS84 (degrees lon, lat).
pub fn webmercator_to_wgs84(mx: f64, my: f64) -> (f64, f64) {
    let lon = mx.to_degrees() / WM_R;
    let lat = (2.0 * (my / WM_R).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
    (lon, lat)
}

/// WebMercator bbox (meters) → WGS84 bbox [west, south, east, north] (degrees).
pub fn webmercator_bbox_to_wgs84(bbox: &Bbox) -> [f64; 4] {
    let (west, south) = webmercator_to_wgs84(bbox.x_min, bbox.y_min);
    let (east, north) = webmercator_to_wgs84(bbox.x_max, bbox.y_max);
    [west, south, east, north]
}

/// WGS84 (degrees) → WebMercator (meters).
// ─── UTM projections via proj4rs ────────────────────────────────────────────

/// Build a proj4 string for a UTM zone given an EPSG code.
/// Supports EPSG 32601–32660 (North) and 32701–32760 (South).
pub fn epsg_to_proj_string(epsg: u32) -> anyhow::Result<String> {
    if epsg >= 32601 && epsg <= 32660 {
        let zone = epsg - 32600;
        Ok(format!(
            "+proj=utm +zone={zone} +datum=WGS84 +units=m +no_defs"
        ))
    } else if epsg >= 32701 && epsg <= 32760 {
        let zone = epsg - 32700;
        Ok(format!(
            "+proj=utm +zone={zone} +south +datum=WGS84 +units=m +no_defs"
        ))
    } else {
        anyhow::bail!("unsupported EPSG {epsg}: only UTM zones 1–60 N/S are supported")
    }
}

/// WGS84 (degrees) → UTM (easting, northing) for the given EPSG code.
pub fn wgs84_to_utm(lon: f64, lat: f64, epsg: u32) -> Result<(f64, f64)> {
    use proj4rs::{transform::transform, Proj};
    let wgs84 = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")?;
    let utm = Proj::from_proj_string(&epsg_to_proj_string(epsg)?)?;
    let mut pt = (lon.to_radians(), lat.to_radians(), 0.0_f64);
    transform(&wgs84, &utm, &mut pt)?;
    Ok((pt.0, pt.1))
}

/// UTM (easting, northing) → WGS84 (degrees lon, lat) for the given EPSG.
/// Convert a WebMercator bbox to UTM (for the given EPSG code).
pub fn webmercator_bbox_to_utm(bbox: &Bbox, epsg: u32) -> Result<Bbox> {
    // Transform all 4 corners and take the envelope
    let corners = [
        webmercator_to_wgs84(bbox.x_min, bbox.y_min),
        webmercator_to_wgs84(bbox.x_max, bbox.y_min),
        webmercator_to_wgs84(bbox.x_min, bbox.y_max),
        webmercator_to_wgs84(bbox.x_max, bbox.y_max),
    ];
    let mut x_min = f64::MAX;
    let mut y_min = f64::MAX;
    let mut x_max = f64::MIN;
    let mut y_max = f64::MIN;
    for (lon, lat) in corners {
        let (e, n) = wgs84_to_utm(lon, lat, epsg)?;
        x_min = x_min.min(e);
        y_min = y_min.min(n);
        x_max = x_max.max(e);
        y_max = y_max.max(n);
    }
    Ok(Bbox {
        x_min,
        y_min,
        x_max,
        y_max,
    })
}

// ─── Affine transform ───────────────────────────────────────────────────────

/// Simple north-up affine transform (no rotation).
#[derive(Debug, Clone, Copy)]
pub struct Affine {
    /// X coordinate of the top-left corner (west edge of pixel 0,0)
    pub origin_x: f64,
    /// Y coordinate of the top-left corner (north edge of pixel 0,0)
    pub origin_y: f64,
    /// Pixel width in CRS units (positive)
    pub pixel_width: f64,
    /// Pixel height in CRS units (positive; Y decreases going down)
    pub pixel_height: f64,
}

impl Affine {
    /// CRS coordinates → fractional pixel (col, row).
    pub fn crs_to_pixel(&self, x: f64, y: f64) -> (f64, f64) {
        let col = (x - self.origin_x) / self.pixel_width - 0.5;
        let row = (self.origin_y - y) / self.pixel_height - 0.5;
        (col, row)
    }
}

// ─── Bilinear resampling ────────────────────────────────────────────────────

use ndarray::Array2;

/// Sample a single-band array at fractional (col, row) using bilinear interpolation.
/// Returns 0 if out of bounds or all 4 neighbors are NODATA (0).
/// NODATA neighbors are excluded and weights are renormalized over the remaining valid
/// neighbors — this avoids dark-fringe blending at scene boundaries while keeping
/// edge pixels valid (no coverage gap at granule seams).
pub fn bilinear_sample_u16(src: &Array2<u16>, col: f64, row: f64) -> u16 {
    let h = src.shape()[0];
    let w = src.shape()[1];

    if col < 0.0 || row < 0.0 || col >= (w as f64) || row >= (h as f64) {
        return 0;
    }

    let r0 = row.floor() as usize;
    let c0 = col.floor() as usize;
    let r1 = (r0 + 1).min(h - 1);
    let c1 = (c0 + 1).min(w - 1);

    let v00 = src[[r0, c0]];
    let v01 = src[[r0, c1]];
    let v10 = src[[r1, c0]];
    let v11 = src[[r1, c1]];

    let dr = row - r0 as f64;
    let dc = col - c0 as f64;

    // Renormalize bilinear weights to skip NODATA (0) neighbors.
    // This avoids blending valid reflectance with out-of-footprint zeros (no dark fringe)
    // while also not zeroing out valid edge pixels that happen to have a NODATA neighbor
    // (no coverage gap at granule boundaries).
    let weights = [
        ((1.0 - dc) * (1.0 - dr), v00),
        (dc * (1.0 - dr), v01),
        ((1.0 - dc) * dr, v10),
        (dc * dr, v11),
    ];
    let (total_w, weighted_sum) = weights
        .iter()
        .filter(|(_, v)| *v != 0)
        .fold((0.0f64, 0.0f64), |(tw, ws), (w, v)| {
            (tw + w, ws + w * *v as f64)
        });

    if total_w == 0.0 {
        return 0;
    }

    (weighted_sum / total_w).round() as u16
}

/// Nearest-neighbour sample for u8 SCL data.
pub fn nn_sample_u8(src: &Array2<u8>, col: f64, row: f64) -> u8 {
    let h = src.shape()[0];
    let w = src.shape()[1];
    let r = row.round() as usize;
    let c = col.round() as usize;
    if r < h && c < w {
        src[[r, c]]
    } else {
        0
    }
}

/// Precompute a WebMercator→UTM coordinate grid for all output pixels.
///
/// Returns a flat `Vec<(f64, f64)>` of (utm_x, utm_y) indexed as `row * output_size + col`.
/// Invalid projections are stored as `(f64::NAN, f64::NAN)`.
/// Call this once per scene; reuse the result for all bands and SCL.
pub fn precompute_wm_to_utm_grid(
    tile_bbox: &Bbox,
    epsg: u32,
    output_size: u32,
) -> Result<Vec<(f64, f64)>> {
    use proj4rs::{transform::transform, Proj};
    let n = output_size as usize;
    let dst_pw = tile_bbox.width() / output_size as f64;
    let dst_ph = tile_bbox.height() / output_size as f64;

    let wgs84 = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")?;
    let utm = Proj::from_proj_string(&epsg_to_proj_string(epsg)?)?;

    let mut grid = Vec::with_capacity(n * n);
    for row in 0..n {
        for col in 0..n {
            let wm_x = tile_bbox.x_min + (col as f64 + 0.5) * dst_pw;
            let wm_y = tile_bbox.y_max - (row as f64 + 0.5) * dst_ph;
            let (lon, lat) = webmercator_to_wgs84(wm_x, wm_y);
            let mut pt = (lon.to_radians(), lat.to_radians(), 0.0_f64);
            if transform(&wgs84, &utm, &mut pt).is_ok() {
                grid.push((pt.0, pt.1));
            } else {
                grid.push((f64::NAN, f64::NAN));
            }
        }
    }
    Ok(grid)
}

/// Warp a u16 band using a precomputed UTM grid (from `precompute_wm_to_utm_grid`).
pub fn warp_band_with_grid(
    src: &Array2<u16>,
    src_affine: &Affine,
    grid: &[(f64, f64)],
    output_size: u32,
) -> Array2<u16> {
    let n = output_size as usize;
    let mut dst = Array2::<u16>::zeros((n, n));
    for row in 0..n {
        for col in 0..n {
            let (utm_x, utm_y) = grid[row * n + col];
            if utm_x.is_nan() {
                continue;
            }
            let (src_col, src_row) = src_affine.crs_to_pixel(utm_x, utm_y);
            dst[[row, col]] = bilinear_sample_u16(src, src_col, src_row);
        }
    }
    dst
}

/// Warp a u8 band (e.g. SCL) using a precomputed UTM grid, nearest-neighbour.
pub fn warp_scl_with_grid(
    src: &Array2<u8>,
    src_affine: &Affine,
    grid: &[(f64, f64)],
    output_size: u32,
) -> Array2<u8> {
    let n = output_size as usize;
    let mut dst = Array2::<u8>::zeros((n, n));
    for row in 0..n {
        for col in 0..n {
            let (utm_x, utm_y) = grid[row * n + col];
            if utm_x.is_nan() {
                continue;
            }
            let (src_col, src_row) = src_affine.crs_to_pixel(utm_x, utm_y);
            dst[[row, col]] = nn_sample_u8(src, src_col, src_row);
        }
    }
    dst
}

/// Warp a single-band u16 array from UTM (src_affine) to a WebMercator 256×256 grid.
///
// ─── Quadkey index helpers ──────────────────────────────────────────────────

/// Encode (x, y) tile at zoom z into a u64 quadkey.
pub fn tile_to_quadkey(x: u32, y: u32, z: u8) -> u64 {
    let mut qk = 0u64;
    for i in (0..z).rev() {
        let mask = 1u32 << i;
        let bit = if y & mask != 0 { 2u64 } else { 0 }
            | if x & mask != 0 { 1u64 } else { 0 };
        qk = qk * 4 + bit;
    }
    qk
}

/// Convert WGS84 (lon, lat) to tile (x, y) at zoom z.
pub fn wgs84_to_tile(lon: f64, lat: f64, z: u8) -> (u32, u32) {
    let n = (1u32 << z) as f64;
    let x = ((lon + 180.0) / 360.0 * n).floor() as u32;
    let lat_rad = lat.to_radians();
    let y = ((1.0
        - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI)
        / 2.0
        * n)
        .floor() as u32;
    (x.min(n as u32 - 1), y.min(n as u32 - 1))
}

/// Return all quadkeys at zoom `qz` that cover tile (z, x, y).
pub fn tile_to_covering_quadkeys(z: u8, x: u32, y: u32, qz: u8) -> Vec<u64> {
    if qz <= z {
        let diff = z - qz;
        let qx = x >> diff;
        let qy = y >> diff;
        vec![tile_to_quadkey(qx, qy, qz)]
    } else {
        let diff = qz - z;
        let min_qx = x << diff;
        let min_qy = y << diff;
        let count = 1u32 << diff;
        let mut qks = Vec::with_capacity((count * count) as usize);
        for qy in min_qy..min_qy + count {
            for qx in min_qx..min_qx + count {
                qks.push(tile_to_quadkey(qx, qy, qz));
            }
        }
        qks
    }
}

/// Return all quadkeys at zoom `qz` that intersect a WGS84 bbox [west, south, east, north].
pub fn bbox_to_quadkeys(bbox: [f64; 4], qz: u8) -> Vec<u64> {
    let [west, south, east, north] = bbox;
    let (min_tx, min_ty) = wgs84_to_tile(west, north, qz); // north → smaller y
    let (max_tx, max_ty) = wgs84_to_tile(east, south, qz); // south → larger y
    let mut qks = Vec::new();
    for tx in min_tx..=max_tx {
        for ty in min_ty..=max_ty {
            qks.push(tile_to_quadkey(tx, ty, qz));
        }
    }
    qks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xyz_to_webmercator_z0() {
        let bbox = xyz_to_webmercator(0, 0, 0);
        assert!((bbox.x_min - (-WM_HALF_SIZE)).abs() < 1.0);
        assert!((bbox.x_max - WM_HALF_SIZE).abs() < 1.0);
        assert!((bbox.y_min - (-WM_HALF_SIZE)).abs() < 1.0);
        assert!((bbox.y_max - WM_HALF_SIZE).abs() < 1.0);
    }

    #[test]
    fn test_quadkey_z1() {
        // Tile (0,0) at z=1 → quadkey 0; (1,0) → 1; (0,1) → 2; (1,1) → 3
        assert_eq!(tile_to_quadkey(0, 0, 1), 0);
        assert_eq!(tile_to_quadkey(1, 0, 1), 1);
        assert_eq!(tile_to_quadkey(0, 1, 1), 2);
        assert_eq!(tile_to_quadkey(1, 1, 1), 3);
    }

    #[test]
    fn test_covering_quadkeys_same_zoom() {
        let qks = tile_to_covering_quadkeys(8, 128, 100, 8);
        assert_eq!(qks.len(), 1);
        assert_eq!(qks[0], tile_to_quadkey(128, 100, 8));
    }

    #[test]
    fn test_covering_quadkeys_tile_below_qz() {
        // tile at z=6, qz=8: each tile covers 4×4 quadkeys
        let qks = tile_to_covering_quadkeys(6, 32, 20, 8);
        assert_eq!(qks.len(), 16);
    }

    #[test]
    fn test_wgs84_to_utm18n_massachusetts() {
        // lon=-71.71, lat=43.0 → UTM18N: expected ~766000 easting, ~4766000 northing
        let (e, n) = wgs84_to_utm(-71.71, 43.0, 32618).unwrap();
        println!("UTM18N: e={e:.0}, n={n:.0}");
        // False easting = 500000; central meridian -75°; offset = 3.29° ≈ 266000m
        assert!((e - 766000.0).abs() < 5000.0, "easting {e} not near 766000");
        assert!((n - 4766000.0).abs() < 5000.0, "northing {n} not near 4766000");
    }
}
