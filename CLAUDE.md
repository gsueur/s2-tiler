# s2-tiler — Claude Code context

Standalone async Rust S2 tile server. No GDAL, no Python. Replaces a titiler-based Python stack that
had 3.5–8s/tile due to synchronous GDAL `/vsicurl/` I/O.

## Purpose

Given a YAML config (extent, years, season, cloud cover, bands), the server:
1. Searches Earth Search STAC for S2 L2A scenes
2. Builds a quadkey spatial index (sorted by cloud cover)
3. Serves XYZ PNG tiles via axum, compositing scenes with per-pixel SCL masking

Target: <800ms cold tile, <300ms warm tile from co-located AWS infra.
Actual (local dev → S3 us-west-2): ~1.1s cold, **10-13ms warm** (tile response cache).

---

## Repository layout

```
s2-tiler/
├── Cargo.toml
├── config.yaml          — Massachusetts JJA 2023 example config (port 3000)
└── src/
    ├── main.rs          — clap CLI, startup, index load/build, axum serve
    ├── config.rs        — S2Config from YAML (serde_yaml); band_to_asset mapping
    ├── stac.rs          — async STAC search (reqwest), paginated, cloud-cover sorted; StacItem has bbox field
    ├── index.rs         — MosaicIndex: quadkey(u64)→Vec<SceneRef>; build_index uses per-item STAC bbox
    ├── cog.rs           — CogReader: async-tiff + object_store HTTP; DashMap IFD cache; shared store by hostname
    ├── geo.rs           — tile math, WebMercator↔WGS84↔UTM (proj4rs), bilinear warp, quadkey math
    ├── composite.rs     — SCL masking (classes 4,5,6,7 valid), best_pixel, median
    ├── render.rs        — rescale u16→u8, PNG/JPEG encode (image 0.24)
    ├── pipeline.rs      — render_tile: index lookup → parallel COG reads → warp → composite
    └── server.rs        — axum routes; AppState with tile_cache (DashMap); cache invalidated on POST /build
```

---

## Key crate versions — DO NOT CHANGE without checking compatibility

| Crate | Version | Note |
|---|---|---|
| `async-tiff` | `0.2` | Uses object_store 0.13 internally |
| `object_store` | `0.13` | **Must match** async-tiff's internal version |
| `reqwest` | `0.13` | Feature is `"rustls"` NOT `"rustls-tls"` (renamed in 0.13) |
| `image` | `0.24` | Pinned; 0.25+ requires Rust ≥1.88 |
| `axum` | `0.8` | — |
| `proj4rs` | `0.1` | Pure Rust CRS, no libproj dep |
| `ndarray` | `0.15` | — |
| `dashmap` | `6` | — |

---

## Architecture decisions and pitfalls

### Spatial index
- `build_index` in `index.rs` uses each STAC item's own `bbox` field (WGS84) to assign scenes to
  quadkey cells. Previously used config extent for all → all 107 scenes assigned to all 8 cells → empty tiles.
- `StacItem.bbox` is `Option<[f64; 4]>` (minx, miny, maxx, maxy); falls back to config extent if absent.

### COG reading (`cog.rs`)

**HTTP store sharing**: All S2 COGs are on `sentinel-cogs.s3.us-west-2.amazonaws.com`. A single
`ObjectStore` per hostname is cached in `CogReader.stores` (DashMap). Previously created one store
per URL → 24 separate TLS handshakes per tile. Now 1 handshake total. Do NOT use `with_http2_only()`
— S3 returns "frame with invalid size" HTTP/2 errors.

**IFD cache**: `CogReader.cache: DashMap<String, Arc<CachedCog>>` caches the parsed TIFF + ObjectReader
per URL. Header fetches happen once per unique COG URL per server lifetime.

**Overview selection**: `select_overview` picks the coarsest overview whose GSD ≤ `desired_gsd * 2.0`
(2x tolerance). This allows one level of upsampling at the cost of slightly lower sharpness, but
significantly reduces tiles to fetch at low zoom levels. IFDs often lack `ModelPixelScale`; GSD is
inferred from `full_gsd * (full_w / ovr_w)`.

**Affine handling**: Overview IFDs often lack both `ModelPixelScale` and `ModelTiepoint`. Use
`ifd_to_affine_with_fallback(ifd, all_ifds)` which falls back to IFD[0] tiepoint + inferred scale.
`window_origin_affine()` computes an affine whose origin is the NW corner of the requested window
(not the full granule) — this is what callers use for warp.

**Tile layout**: S2 COGs are chunky (h, w, bands) for single-band files. Shape detection:
`if shape[2] <= shape[0] { chunky } else { planar }`. `pixel_bytes = data_type.size()` (2 for u16).
Bytes are native-endian (little-endian on x86). DEFLATE tiles that fall outside the scene footprint
compress to ~2055 bytes (all zeros).

### Warp (`geo.rs`)

**Precomputed grid** (critical for performance): Before warping, call `precompute_wm_to_utm_grid`
once per scene to get a flat `Vec<(f64, f64)>` of UTM coordinates for all 256×256 output pixels.
Reuse it for `warp_band_with_grid` (bilinear, u16) and `warp_scl_with_grid` (nearest-neighbour, u8).
Without this, each band call would run 65536 proj4rs transforms; with it, 1 pass total per scene.

`proj4rs` inputs are in **radians**: `lon.to_radians()` before `transform()`.

**SCL masking**: valid classes = {4=vegetation, 5=bare soil, 6=water, 7=low-prob-cloud}.
Applied in `composite.rs::apply_scl_mask`.

### Tile response cache (`server.rs`)

`AppState.tile_cache: DashMap<(u8, u32, u32, String), Bytes>` — keyed by (z, x, y, format string).
Only non-empty tiles are cached (empty/`Ok(None)` responses are not). Cleared on `POST /build`.
Warm hits: ~10-13ms from localhost.

---

## API

```
GET  /{z}/{x}/{y}?format=png|jpg   — tile (default format: png)
GET  /tilejson.json                 — TileJSON spec
GET  /config                        — active S2Config as JSON
GET  /info                          — scene count, index cells, zoom range
POST /build                         — re-run STAC search + rebuild index
```

---

## Config (`config.yaml`)

```yaml
extent: [-73.6, 41.2, -69.8, 42.9]  # WGS84 [west, south, east, north]
years: [2023]
season: [6, 7, 8]                    # months; omit for full year
max_cloud_cover: 20
bands: [B04, B03, B02]               # true color RGB
composite: best_pixel                # best_pixel | median | latest
rescale: [0, 3000]                   # S2 L2A SR → [0,255]
minzoom: 10
maxzoom: 15
quadkey_zoom: 8
stac_url: https://earth-search.aws.element84.com/v1
collection: sentinel-2-l2a
port: 3000
max_scenes_per_tile: 6
```

**Band → asset key mapping** (Earth Search v1):
`B02→blue, B03→green, B04→red, B05→rededge1, B06→rededge2, B07→rededge3, B08→nir, B8A→nir08, B11→swir16, B12→swir22, SCL→scl`

**Rescale presets**:
- True color (B04/B03/B02): `[0, 3000]`
- False color NIR (B08/B04/B03): `[0, 4000]`
- SWIR (B12/B08/B04): `[0, 5000]`

---

## Running

```bash
# Build release binary (~8MB, ~4s build)
cargo build --release

# Start server (searches STAC + builds index on startup, ~3s)
RUST_LOG=warn ./target/release/s2-tiler --config config.yaml

# With index persistence (skips STAC search on restart if file exists)
./target/release/s2-tiler --config config.yaml --index-path index.json

# Force rebuild even if index file exists
./target/release/s2-tiler --config config.yaml --index-path index.json --rebuild

# Dev: verbose logging
RUST_LOG=s2_tiler=debug ./target/release/s2-tiler --config config.yaml
```

---

## Benchmarks (local dev → S3 us-west-2)

| Scenario | Latency | Notes |
|---|---|---|
| True cold (no caches) | ~1.1s | Headers + pixel fetch; was 3.3s before store sharing |
| Header-warm, new tile | ~1.2–1.7s | Pixel fetch only |
| Tile cache hit | **10–13ms** | Served from DashMap, all zoom levels |
| Python baseline (titiler) | 3.5–8s | GDAL /vsicurl/ blocking I/O |

On co-located AWS infra (same region as S3), cold tiles would be ~150–400ms.

---

## Massachusetts smoke test

```bash
# Valid tile coordinates (center of extent, JJA 2023)
curl -o /tmp/z10.png "http://localhost:3000/10/308/378?format=png"
curl -o /tmp/z12.png "http://localhost:3000/12/1234/1514?format=png"
curl -o /tmp/z14.png "http://localhost:3000/14/4936/6056?format=png"

# Expected: non-empty PNG, 17–130KB depending on zoom
ls -la /tmp/z*.png

# QGIS: add XYZ layer
# URL: http://localhost:3000/{z}/{x}/{y}?format=png
# Tile size: 256, min zoom: 10, max zoom: 15
```

---

## Unit tests

```bash
cargo test
```

Tests cover: tile math (z=0 bbox, quadkey encoding), UTM projection (Massachusetts, UTM18N),
quadkey covering functions.

---

## Known issues / TODO

- **No tile size config**: hardcoded to 256px in `pipeline.rs::TILE_SIZE`
- **No TTL on tile cache**: tiles are cached indefinitely until `POST /build`; suitable for
  static seasonal composites, not near-realtime use
- **Median composite is expensive**: reads all scenes regardless of pixel fill; avoid for
  large extents or many scenes
- **Index file format**: DuckDB (bundled, `duckdb = "1"`). Schema: `meta`, `scenes` (bands as JSON
  column), `scene_quadkeys` with index on `quadkey`. First build after adding the dep takes ~2min
  (C++ compile); subsequent builds are incremental. File is overwritten on each save.
- **No authentication**: COGs are fetched from public S3 (Element84 Earth Search); for MPC
  (Azure Planetary Computer), assets require signing via `planetary_computer.sign()`
