/// Pixel compositing: SCL masking, best_pixel, median.
use crate::config::Composite;
use ndarray::{Array2, Array3};

/// SCL classes considered valid (clear) pixels.
const VALID_SCL: &[u8] = &[4, 5, 6, 7]; // vegetation, bare soil, water, low-cloud-prob

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
    /// Valid pixel mask: shape (256, 256), true = pixel is valid (not cloud/nodata)
    pub mask: Array2<bool>,
}

impl SceneTile {
    pub fn empty(bands: usize, size: usize) -> Self {
        Self {
            data: Array3::zeros((bands, size, size)),
            mask: Array2::from_elem((size, size), false),
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
pub fn apply_scl_mask(data: Array3<u16>, scl: &Array2<u8>) -> SceneTile {
    let h = data.shape()[1];
    let w = data.shape()[2];
    let mask = Array2::from_shape_fn((h, w), |(r, c)| scl_is_valid(scl[[r, c]]));
    SceneTile { data, mask }
}

/// Composite multiple scene tiles using the configured strategy.
/// `scenes` must be ordered by cloud cover ascending for best_pixel / latest.
pub fn composite(scenes: Vec<SceneTile>, strategy: &Composite) -> SceneTile {
    if scenes.is_empty() {
        return SceneTile::empty(3, 256);
    }

    match strategy {
        Composite::BestPixel | Composite::Latest => best_pixel(scenes),
        Composite::Median => median(scenes),
    }
}

/// First-valid-pixel composite (cloud-cover-ordered input → least cloudy wins).
fn best_pixel(scenes: Vec<SceneTile>) -> SceneTile {
    let bands = scenes[0].bands();
    let size = scenes[0].size();

    let mut result_data = Array3::<u16>::zeros((bands, size, size));
    let mut result_mask = Array2::<bool>::from_elem((size, size), false);

    for scene in &scenes {
        for row in 0..size {
            for col in 0..size {
                if !result_mask[[row, col]] && scene.mask[[row, col]] {
                    for b in 0..bands {
                        result_data[[b, row, col]] = scene.data[[b, row, col]];
                    }
                    result_mask[[row, col]] = true;
                }
            }
        }
        // Early exit if all pixels are filled
        if result_mask.iter().all(|&v| v) {
            break;
        }
    }

    SceneTile {
        data: result_data,
        mask: result_mask,
    }
}

/// Median composite across all valid-pixel samples.
fn median(scenes: Vec<SceneTile>) -> SceneTile {
    let bands = scenes[0].bands();
    let size = scenes[0].size();

    let mut result_data = Array3::<u16>::zeros((bands, size, size));
    let mut result_mask = Array2::<bool>::from_elem((size, size), false);

    for row in 0..size {
        for col in 0..size {
            for b in 0..bands {
                let valid_vals: Vec<u16> = scenes
                    .iter()
                    .filter(|s| s.mask[[row, col]])
                    .map(|s| s.data[[b, row, col]])
                    .collect();

                if !valid_vals.is_empty() {
                    result_mask[[row, col]] = true;
                    result_data[[b, row, col]] = median_u16(&valid_vals);
                }
            }
        }
    }

    SceneTile {
        data: result_data,
        mask: result_mask,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_scene(val: u16, valid: bool, size: usize) -> SceneTile {
        let data = Array3::from_elem((3, size, size), val);
        let mask = Array2::from_elem((size, size), valid);
        SceneTile { data, mask }
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
