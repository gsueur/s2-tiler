# s2-tiler

Standalone async Sentinel-2 COG tile server. No GDAL, no Python.

Searches Earth Search STAC for Sentinel-2 L2A scenes, builds a spatial index, and serves XYZ PNG/JPEG tiles via HTTP by reading Cloud-Optimized GeoTIFFs directly from S3. Multiple named tilesets are served from a single process, each with its own spatial extent, date range, and band configuration.

**Performance (local dev → S3 us-west-2)**

| Scenario | Latency |
|---|---|
| True cold (no caches) | ~1.1s |
| Header-warm, pixel fetch | ~1.2–1.7s |
| Tile cache hit (memory) | 10–13ms |
| Python/titiler baseline | 3.5–8s |

On co-located AWS infrastructure (same region as the S3 bucket), cold tiles are typically 150–400ms.

---

## Requirements

- Rust 1.85+
- No system dependencies — DuckDB is bundled, CRS transforms are pure Rust (no libproj)

---

## Build

```bash
cargo build --release
# Binary: ./target/release/s2-tiler (~8MB, first build ~4–5min due to bundled DuckDB)
```

---

## Quick start

```bash
# Start server — searches STAC and builds the spatial index on first run (~3s)
./target/release/s2-tiler --config config.yaml

# With index persistence (skips STAC search on restart if tilesets.db exists)
# Set index_path in config.yaml (see Configuration section)

# Force rebuild even if index file exists
./target/release/s2-tiler --config config.yaml --rebuild

# Verbose logging
RUST_LOG=s2_tiler=debug ./target/release/s2-tiler --config config.yaml
```

Add as an XYZ layer in QGIS or any slippy map client:
```
http://localhost:3000/{name}/{z}/{x}/{y}?format=png
```

---

## Configuration

All configuration lives in a single YAML file. Global settings apply to all tilesets; each tileset only needs its spatial and temporal parameters.

```yaml
# ── Global ───────────────────────────────────────────────────────────────────

port: 3000
minzoom: 10
maxzoom: 15
quadkey_zoom: 8                                         # spatial index resolution
stac_url: https://earth-search.aws.element84.com/v1     # Earth Search STAC API
collection: sentinel-2-l2a

# Shared DuckDB file for all mosaic indices (optional; omit to disable persistence)
index_path: tilesets.db

# Tile cache backend (optional; default: memory)
tile_cache:
  backend: memory   # none | memory | local | duckdb | s3 | r2

# ── Tilesets ─────────────────────────────────────────────────────────────────

tilesets:
  - name: massachusetts                         # used as URL prefix
    extent: [-73.6, 41.2, -69.8, 42.9]         # [west, south, east, north] WGS84
    years: [2023, 2024, 2025]
    season: [6, 7, 8]                           # months; omit for full year
    max_cloud_cover: 20                         # percent
    bands: [B04, B03, B02]                      # true color RGB
    composite: best_pixel                       # best_pixel | median | latest
    rescale: [0, 3000]                          # S2 L2A SR → [0, 255]
    max_scenes_per_tile: 12
```

### Tile cache backends

**`memory`** (default) — DashMap; fast, lost on process restart.

**`none`** — no caching; renders every tile on every request.

**`local`** — tiles written to disk as `{path}/{tileset}/{z}/{x}/{y}.{format}`.
```yaml
tile_cache:
  backend: local
  path: /var/cache/s2-tiler
```

**`duckdb`** — tiles stored in a `tile_cache` table in a DuckDB file. Can share the same file as `index_path`.
```yaml
tile_cache:
  backend: duckdb
  path: tilesets.db
```

**`s3`** — tiles written to S3. Credentials from environment variables `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`.
```yaml
tile_cache:
  backend: s3
  bucket: my-tiles
  region: us-east-1
  prefix: tiles       # optional, default: "tiles"
```

**`r2`** — Cloudflare R2 (S3-compatible). Credentials from `R2_ACCESS_KEY_ID` and `R2_SECRET_ACCESS_KEY`.
```yaml
tile_cache:
  backend: r2
  bucket: my-tiles
  account_id: abc123def456
  prefix: tiles       # optional, default: "tiles"
```

### Band codes and rescale presets

| Band | Asset | Description |
|---|---|---|
| B02 | blue | Blue |
| B03 | green | Green |
| B04 | red | Red |
| B05–B07 | rededge1–3 | Red edge |
| B08 | nir | NIR broad |
| B8A | nir08 | NIR narrow |
| B11 | swir16 | SWIR 1.6µm |
| B12 | swir22 | SWIR 2.2µm |
| SCL | scl | Scene classification (used internally for masking) |

Common band combinations and rescale values:

| Composite | Bands | Rescale |
|---|---|---|
| True color | B04, B03, B02 | [0, 3000] |
| False color NIR | B08, B04, B03 | [0, 4000] |
| SWIR | B12, B08, B04 | [0, 5000] |
| NDVI | B08, B04 | [-1, 1] or [0, 1] |

**NDVI** requires `composite: ndvi` and exactly 2 bands in order `[NIR, Red]`. The `rescale` values are interpreted as float NDVI units, not SR units. Use `[-1, 1]` to show the full index range, or `[0, 1]` to emphasise vegetation. Pixels where NIR + Red = 0 are treated as nodata.

---

## HTTP API

All tile endpoints are prefixed with the tileset name.

```
GET  /{name}/{z}/{x}/{y}?format=png|jpg   — tile (default: png)
GET  /{name}/tilejson.json                — TileJSON 2.2
GET  /{name}/config                       — active tileset config as JSON
GET  /{name}/info                         — scene count, index cells, zoom range
POST /{name}/build                        — re-run STAC search, rebuild index, clear tile cache
GET  /tilesets                            — list all configured tileset names
```

`POST /{name}/build` is safe to call at runtime — it updates the index and clears cached tiles for that tileset only, without affecting other tilesets.

---

## Multi-tileset example

```yaml
port: 3000
minzoom: 10
maxzoom: 15
quadkey_zoom: 8
stac_url: https://earth-search.aws.element84.com/v1
collection: sentinel-2-l2a
index_path: tilesets.db

tile_cache:
  backend: r2
  bucket: my-tiles
  account_id: abc123def456

tilesets:
  - name: massachusetts
    extent: [-73.6, 41.2, -69.8, 42.9]
    years: [2023, 2024, 2025]
    season: [6, 7, 8]
    max_cloud_cover: 20
    bands: [B04, B03, B02]
    composite: best_pixel
    rescale: [0, 3000]
    max_scenes_per_tile: 12

  - name: california-falsecolor
    extent: [-124.5, 32.5, -114.1, 42.1]
    years: [2024]
    season: [7, 8, 9]
    max_cloud_cover: 15
    bands: [B08, B04, B03]
    composite: best_pixel
    rescale: [0, 4000]
    max_scenes_per_tile: 8
```

Tiles served at:
- `http://localhost:3000/massachusetts/{z}/{x}/{y}?format=png`
- `http://localhost:3000/california-falsecolor/{z}/{x}/{y}?format=png`

---

## Architecture

```
config.yaml
    └── AppConfig (port, global settings, tilesets[])
            │
            ▼
main.rs  ──  STAC search (stac.rs)
         ──  build MosaicIndex (index.rs) → persisted to DuckDB (tilesets.db)
         ──  axum HTTP server (server.rs)
                │
                ├── /{name}/{z}/{x}/{y}
                │       tile cache lookup (cache.rs)
                │       → index lookup (index.rs)
                │       → parallel COG reads (cog.rs, async-tiff, object_store)
                │       → reproject + warp to WebMercator (geo.rs, proj4rs)
                │       → SCL masking + composite (composite.rs)
                │       → rescale + PNG/JPEG encode (render.rs)
                │       → tile cache write
                │
                └── POST /{name}/build
                        re-search STAC → rebuild index → clear tile cache
```

**Spatial index** — quadkey grid at `quadkey_zoom` (default 8). Each scene is indexed against its own STAC bbox, clipped to the tileset extent. Scenes are sorted by cloud cover ascending within each cell.

**COG reading** — `async-tiff` reads IFDs and pixel windows directly from S3 over HTTP, with one shared `object_store` connection per hostname (avoids redundant TLS handshakes). IFDs are cached per URL for the server lifetime. Overview selection allows up to 2x upsampling to reduce fetched tile count at low zoom levels.

**Warp** — per-scene, a 256×256 UTM→WebMercator coordinate grid is precomputed once, then reused for all band warps (bilinear) and the SCL warp (nearest-neighbour). This amortizes the proj4rs transform cost across bands.

**Compositing** — `best_pixel` picks the lowest-cloud-cover valid pixel across scenes. Valid SCL classes: 4 (vegetation), 5 (bare soil), 6 (water), 7 (low-probability cloud). `median` is expensive for large extents.

---

## Known limitations

- Tile size is hardcoded to 256px.
- Tile cache has no TTL. The `memory` and `duckdb` backends grow indefinitely until `POST /{name}/build`. Use `none` or manage externally for near-realtime data.
- `median` composite reads all scenes regardless of fill; avoid for large extents or many scenes.
- No authentication. COGs are fetched from public S3 (Element84 Earth Search). For Microsoft Planetary Computer, assets require signing via their token API before use.
- Large extents with many years (e.g. Europe × 5 years) can produce DuckDB index files of 50–200MB; this is fine but loading is slower than small configs.
