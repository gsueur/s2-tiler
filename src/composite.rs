/// Pixel compositing: SCL masking, best_pixel, median.
use crate::config::Composite;
use ndarray::{Array2, Array3};
use std::collections::VecDeque;

/// Check if an SCL value is valid (not cloud/shadow/snow/nodata).
#[inline]
pub fn scl_is_valid(scl: u8) -> bool {
    matches!(scl, 4 | 5 | 6 | 7)
}

/// A single scene's warped, masked raster ready for compositing.
/// Coordinates are WebMercator 256×256 pixels.
#[derive(Debug)]
pub struct SceneTile {
    /// Spectral bands: shape (num_bands, 256, 256)
    pub data: Array3<u16>,
    /// Valid pixel mask: shape (256, 256), true = pixel has a clear (SCL-valid) observation
    pub mask: Array2<bool>,
    /// Scene footprint: shape (256, 256), true = at least one scene had any SCL data here
    /// (regardless of SCL class). Used by fill_gaps to distinguish cloud holes (fill)
    /// from no-coverage areas (leave transparent).
    pub covered: Array2<bool>,
    /// NDVI values in [-1, 1] — set only when composite = ndvi; render uses this in place of data.
    pub ndvi: Option<Array2<f32>>,
}

impl SceneTile {
    pub fn empty(bands: usize, size: usize) -> Self {
        Self {
            data: Array3::zeros((bands, size, size)),
            mask: Array2::from_elem((size, size), false),
            covered: Array2::from_elem((size, size), false),
            ndvi: None,
        }
    }

    pub fn bands(&self) -> usize {
        self.data.shape()[0]
    }

    pub fn size(&self) -> usize {
        self.data.shape()[1]
    }
}

/// Apply SCL mask to a spectral tile, returning a `SceneTile`.
/// A pixel is valid if:
///   1. Its SCL class is clear (not cloud/shadow/snow/nodata)
///   2. All band values are non-zero (NODATA from bilinear boundary propagation)
///   3. Not all bands exceed `haze_dn_max` (thin haze / unmasked cloud, 0 = disabled)
pub fn apply_scl_mask(data: Array3<u16>, scl: &Array2<u8>, haze_dn_max: u16) -> SceneTile {
    let bands = data.shape()[0];
    let h = data.shape()[1];
    let w = data.shape()[2];
    // covered: the scene has any SCL data here (not out-of-footprint).
    // Includes cloud/shadow/snow pixels — anything the sensor saw.
    let covered = Array2::from_shape_fn((h, w), |(r, c)| scl[[r, c]] != 0);
    let mask = Array2::from_shape_fn((h, w), |(r, c)| {
        if !scl_is_valid(scl[[r, c]]) {
            return false;
        }
        if (0..bands).any(|b| data[[b, r, c]] == 0) {
            return false;
        }
        if haze_dn_max > 0 && (0..bands).all(|b| data[[b, r, c]] > haze_dn_max) {
            return false;
        }
        true
    });
    SceneTile { data, mask, covered, ndvi: None }
}

/// Composite multiple scene tiles using the configured strategy.
/// `scenes` must be ordered by cloud cover ascending for best_pixel / latest.
pub fn composite(scenes: Vec<SceneTile>, strategy: &Composite, n_bands: usize) -> SceneTile {
    if scenes.is_empty() {
        return SceneTile::empty(n_bands, 256);
    }

    match strategy {
        Composite::BestPixel | Composite::Latest => best_pixel(scenes),
        Composite::Median => median(scenes),
        Composite::Ndvi => {
            let composited = best_pixel(scenes);
            compute_ndvi(composited)
        }
    }
}

/// First-valid-pixel composite (cloud-cover-ordered input → least cloudy wins).
fn best_pixel(scenes: Vec<SceneTile>) -> SceneTile {
    let bands = scenes[0].bands();
    let size = scenes[0].size();
    let total = size * size;

    let mut result_data = Array3::<u16>::zeros((bands, size, size));
    let mut result_mask = Array2::<bool>::from_elem((size, size), false);
    let mut result_covered = Array2::<bool>::from_elem((size, size), false);
    let mut filled = 0usize;

    for scene in &scenes {
        for row in 0..size {
            for col in 0..size {
                if scene.covered[[row, col]] {
                    result_covered[[row, col]] = true;
                }
                if !result_mask[[row, col]] && scene.mask[[row, col]] {
                    for b in 0..bands {
                        result_data[[b, row, col]] = scene.data[[b, row, col]];
                    }
                    result_mask[[row, col]] = true;
                    filled += 1;
                }
            }
        }
        if filled == total {
            break;
        }
    }

    SceneTile {
        data: result_data,
        mask: result_mask,
        covered: result_covered,
        ndvi: None,
    }
}

/// Median composite across all valid-pixel samples.
fn median(scenes: Vec<SceneTile>) -> SceneTile {
    let bands = scenes[0].bands();
    let size = scenes[0].size();
    let n_scenes = scenes.len();

    let mut result_data = Array3::<u16>::zeros((bands, size, size));
    let mut result_mask = Array2::<bool>::from_elem((size, size), false);
    // Reusable buffer: one alloc amortized across all (row, col, band) iterations.
    let mut buf: Vec<u16> = Vec::with_capacity(n_scenes);

    for row in 0..size {
        for col in 0..size {
            // Skip pixels with no valid scene data.
            let any_valid = scenes.iter().any(|s| s.mask[[row, col]]);
            if !any_valid {
                continue;
            }
            result_mask[[row, col]] = true;

            for b in 0..bands {
                buf.clear();
                buf.extend(
                    scenes
                        .iter()
                        .filter(|s| s.mask[[row, col]])
                        .map(|s| s.data[[b, row, col]]),
                );
                result_data[[b, row, col]] = median_u16(&buf);
            }
        }
    }

    // covered = union of all scene footprints
    let result_covered = Array2::from_shape_fn((size, size), |(row, col)| {
        scenes.iter().any(|s| s.covered[[row, col]])
    });

    SceneTile {
        data: result_data,
        mask: result_mask,
        covered: result_covered,
        ndvi: None,
    }
}

/// Compute median of a non-empty slice of u16 values.
fn median_u16(vals: &[u16]) -> u16 {
    let mut sorted = vals.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 0 {
        // Average middle two values
        (sorted[n / 2 - 1] as u32 + sorted[n / 2] as u32) as u16 / 2
    } else {
        sorted[n / 2]
    }
}

/// Compute per-pixel NDVI from a 2-band composited tile (band 0 = NIR, band 1 = Red).
///
/// Returns a 1-band SceneTile with `ndvi` set to f32 values in [-1, 1].
/// Invalid pixels (mask = false) get NDVI = 0.0.
fn compute_ndvi(composited: SceneTile) -> SceneTile {
    let size = composited.size();
    let mut ndvi_arr = Array2::<f32>::zeros((size, size));

    for row in 0..size {
        for col in 0..size {
            if composited.mask[[row, col]] {
                let nir = composited.data[[0, row, col]] as f32;
                let red = composited.data[[1, row, col]] as f32;
                let denom = nir + red;
                ndvi_arr[[row, col]] = if denom > 0.0 {
                    ((nir - red) / denom).clamp(-1.0, 1.0)
                } else {
                    0.0
                };
            }
        }
    }

    SceneTile {
        data: Array3::zeros((1, size, size)), // unused when ndvi is Some
        mask: composited.mask,
        covered: composited.covered,
        ndvi: Some(ndvi_arr),
    }
}

/// Fill SCL-masked pixels (mask=false) via BFS nearest-neighbor inpainting.
///
/// Only fills pixels within the scene footprint (`covered=true`) — these are pixels
/// that at least one scene observed but all observations were cloud/shadow/snow.
/// Pixels outside all scene footprints (`covered=false`) are left transparent.
///
/// This correctly handles:
/// - Interior cloud holes in urban areas → filled (covered && !mask)
/// - South/north edge of extent with no coverage → transparent (!covered)
/// - Large cloud masses that touch the tile boundary in covered areas → filled
pub fn fill_gaps(tile: &mut SceneTile) {
    let size = tile.size();
    let bands = tile.bands();

    // Seed BFS from all valid pixels.
    let mut queue: VecDeque<(usize, usize)> = VecDeque::new();
    for r in 0..size {
        for c in 0..size {
            if tile.mask[[r, c]] {
                queue.push_back((r, c));
            }
        }
    }

    if queue.is_empty() {
        return; // No valid pixels at all — nothing to propagate from.
    }

    while let Some((r, c)) = queue.pop_front() {
        for (nr, nc) in [(r.wrapping_sub(1), c), (r + 1, c), (r, c.wrapping_sub(1)), (r, c + 1)] {
            // Only fill pixels that are (a) in-bounds, (b) not yet valid, (c) within a scene footprint.
            if nr < size && nc < size && !tile.mask[[nr, nc]] && tile.covered[[nr, nc]] {
                if let Some(ndvi) = &mut tile.ndvi {
                    ndvi[[nr, nc]] = ndvi[[r, c]];
                } else {
                    for b in 0..bands {
                        tile.data[[b, nr, nc]] = tile.data[[b, r, c]];
                    }
                }
                tile.mask[[nr, nc]] = true;
                queue.push_back((nr, nc));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_scene(val: u16, valid: bool, size: usize) -> SceneTile {
        let data = Array3::from_elem((3, size, size), val);
        let mask = Array2::from_elem((size, size), valid);
        let covered = Array2::from_elem((size, size), true);
        SceneTile { data, mask, covered, ndvi: None }
    }

    #[test]
    fn best_pixel_first_valid_wins() {
        let s1 = make_scene(100, true, 4);
        let s2 = make_scene(200, true, 4);
        let result = best_pixel(vec![s1, s2]);
        assert_eq!(result.data[[0, 0, 0]], 100);
    }

    #[test]
    fn best_pixel_falls_through_invalid() {
        let s1 = make_scene(100, false, 4);
        let s2 = make_scene(200, true, 4);
        let result = best_pixel(vec![s1, s2]);
        assert_eq!(result.data[[0, 0, 0]], 200);
    }

    #[test]
    fn median_correct() {
        assert_eq!(median_u16(&[3, 1, 2]), 2);
        assert_eq!(median_u16(&[4, 2]), 3);
        assert_eq!(median_u16(&[10]), 10);
    }

    #[test]
    fn scl_valid_classes() {
        assert!(scl_is_valid(4));
        assert!(scl_is_valid(5));
        assert!(scl_is_valid(6));
        assert!(scl_is_valid(7));
        assert!(!scl_is_valid(3));
        assert!(!scl_is_valid(8));
        assert!(!scl_is_valid(9));
        assert!(!scl_is_valid(0));
    }
}
