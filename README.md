# s2-tiler

Standalone async Sentinel-2 COG tile server. 

Searches Earth Search STAC for Sentinel-2 L2A scenes, builds a spatial index, and serves XYZ PNG/JPEG tiles via HTTP by reading Cloud-Optimized GeoTIFFs directly from S3. Multiple named tilesets are served from a single process, each with its own spatial extent, date range, and band configuration.

---

## Requirements

- Rust 1.85+
- No system dependencies — DuckDB is bundled, CRS transforms are pure Rust (no libproj)

---

## Build

### Install Rust

**macOS / Linux**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

**Windows**

Download and run [rustup-init.exe](https://win.rustup.rs/) from [rustup.rs](https://rustup.rs), or install via winget:
```powershell
winget install Rustlang.Rustup
```

Verify the installation:
```bash
rustc --version   # should be 1.85 or newer
cargo --version
```

### Compile

```bash
git clone https://github.com/gsueur/s2-tiler.git
cd s2-tiler
cargo build --release
# Binary: ./target/release/s2-tiler  (~8MB)
# First build takes 4–5 minutes — DuckDB compiles from source
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

### CLI flags

| Flag | Default | Description |
|---|---|---|
| `-c, --config` | `config.yaml` | Path to the YAML config file |
| `--rebuild` | false | Force STAC re-search and index rebuild on startup |
| `--host` | `0.0.0.0` | Bind address |
| `--prefetch` | false | Pre-render all tiles into the cache, then exit |
| `--tilesets` | all | Comma-separated tileset names to prefetch (e.g. `massachusetts,miami`) |
| `--concurrency` | 8 | Max concurrent tile chunks during prefetch |
| `--prefetch-format` | `png` | Image format written to cache during prefetch (`png`, `jpg`, `webp`) |
| `--skip-cached` | false | Skip tiles already present in the cache (resume an interrupted prefetch) |

### Pre-seeding the tile cache

The `--prefetch` mode renders every tile in the configured extent and zoom range, writing results to the tile cache. Tiles are processed in 8×8 spatial chunks — one COG read per scene per band covers all 64 tiles in a chunk, rather than one read per tile, which dramatically reduces HTTP requests to S3.

```bash
# Pre-seed all tilesets
./target/release/s2-tiler --config config.yaml --prefetch

# Pre-seed a single tileset with higher concurrency
./target/release/s2-tiler --config config.yaml --prefetch --tilesets massachusetts --concurrency 32

# Resume an interrupted prefetch (skip tiles already written to cache)
./target/release/s2-tiler --config config.yaml --prefetch --tilesets massachusetts --skip-cached
```

Progress is displayed as a live bar with tiles/s throughput and ETA. The process exits after prefetch completes; it does not start the HTTP server.

---

## Configuration

All configuration lives in a single YAML file. Global settings apply to all tilesets; visual parameters (bands, rescale, composite strategy) are defined once as named presets and referenced by tilesets.

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

# ── Visual presets ────────────────────────────────────────────────────────────
# Define bands, rescale, and composite strategy once; reference by name in tilesets.

presets:
  truecolor:
    bands: [B04, B03, B02]
    rescale: [0, 3000]
    composite: best_pixel
  falsecolor:
    bands: [B08, B04, B03]
    rescale: [0, 4000]
    composite: best_pixel

# ── Tilesets ─────────────────────────────────────────────────────────────────

tilesets:
  - name: massachusetts                         # used as URL prefix
    preset: truecolor                           # references a preset defined above
    extent: [-73.6, 41.2, -69.8, 42.9]         # [west, south, east, north] WGS84
    years: [2023, 2024, 2025]
    season: [6, 7, 8]                           # months; omit for full year
    max_cloud_cover: 20                         # percent
    max_scenes_per_tile: 12
    haze_dn_max: 2400                           # optional; 0 = disabled
```

### Preset fields

Presets are defined under the top-level `presets:` key and referenced in tilesets via `preset: <name>`.

| Field | Required | Default | Description |
|---|---|---|---|
| `bands` | yes | — | S2 band codes; 1 (grayscale) or 3 (RGB), or 2 for NDVI |
| `rescale` | no | `[0, 3000]` | Input value range mapped to [0, 255] for display |
| `composite` | no | `best_pixel` | `best_pixel`, `latest`, `median`, or `ndvi` |

### Tileset fields

| Field | Required | Default | Description |
|---|---|---|---|
| `name` | yes | — | Tileset identifier; used as the URL prefix |
| `preset` | yes* | — | Named preset from the `presets:` section (*or set `bands` directly) |
| `extent` | yes | — | `[west, south, east, north]` in WGS84 |
| `years` | yes | — | List of years to include, e.g. `[2023, 2024]` |
| `season` | no | full year | Month numbers, e.g. `[6, 7, 8]` for June–August |
| `max_cloud_cover` | no | 20 | Maximum scene cloud cover percent for STAC pre-filter |
| `max_scenes_per_tile` | no | 6 | Max scenes composited per tile (caps cold-tile latency) |
| `haze_dn_max` | no | 0 (off) | Reject pixels where all bands exceed this DN value (thin haze/cloud). Typical values: 2400 for true-color `[0, 3000]`, 3200 for NIR `[0, 4000]`. Only used when `scl_masking: true` |
| `scl_masking` | no | `true` | When `true`, pixels are filtered by SCL class (4/5/6/7 valid) and `haze_dn_max`. When `false`, any non-zero pixel in the scene footprint is used as-is — avoids false masking of bright surfaces (urban, sand, snow) at the cost of no cloud filtering |
| `temporal_priority` | no | `false` | When `true`, scenes are sorted by most-recent year/month first, then cloud cover within each period. Produces more homogeneous composites — adjacent tiles draw from the same acquisition period before falling back to older scenes |

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

| Name | Bands | Rescale | Use case |
|---|---|---|---|
| True color | `[B04, B03, B02]` | `[0, 3000]` | Natural look |
| False color NIR | `[B08, B04, B03]` | `[0, 4000]` | Vegetation health, land cover (vegetation = red) |
| SWIR | `[B12, B08, B04]` | `[0, 5000]` | Burn scars, active geology, bare soil |
| SWIR-2 | `[B12, B8A, B04]` | `[0, 5000]` | Geology, moisture content |
| Red edge | `[B07, B05, B02]` | `[0, 4000]` | Plant stress, canopy structure |
| NDVI | `[B08, B04]` | `[-1, 1]` or `[0, 1]` | Vegetation index (requires `composite: ndvi`) |

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

presets:
  truecolor:
    bands: [B04, B03, B02]
    rescale: [0, 3000]
    composite: best_pixel
  falsecolor:
    bands: [B08, B04, B03]
    rescale: [0, 4000]
    composite: best_pixel

tilesets:
  - name: massachusetts
    preset: truecolor
    extent: [-73.6, 41.2, -69.8, 42.9]
    years: [2023, 2024, 2025]
    season: [6, 7, 8]
    max_cloud_cover: 20
    max_scenes_per_tile: 12

  - name: california-falsecolor
    preset: falsecolor
    extent: [-124.5, 32.5, -114.1, 42.1]
    years: [2024]
    season: [7, 8, 9]
    max_cloud_cover: 15
    max_scenes_per_tile: 8
```

Tiles served at:
- `http://localhost:3000/massachusetts/{z}/{x}/{y}?format=png`
- `http://localhost:3000/california-falsecolor/{z}/{x}/{y}?format=png`

---

## Tests

```bash
cargo test
```

Covers: tile math (z=0 bbox, quadkey encoding), UTM projection (Massachusetts, UTM18N), quadkey covering functions, SCL valid class detection, best_pixel and median compositing logic.

---

## Architecture

```
config.yaml
    └── AppConfig (port, global settings, tilesets[])
            │
            ▼
main.rs  ──  STAC search (stac.rs)
         ──  build MosaicIndex (index.rs) → persisted to DuckDB (tilesets.db)
         ──  [--prefetch] pre-seed tile cache (prefetch.rs), then exit
         ──  axum HTTP server (server.rs)
                │
                ├── /{name}/{z}/{x}/{y}
                │       tile cache lookup (cache.rs)
                │       → index lookup (index.rs)
                │       → parallel COG reads (cog.rs, async-tiff, object_store)
                │       → reproject + warp to WebMercator (geo.rs, proj4rs)
                │       → SCL masking + haze rejection + composite (composite.rs)
                │       → gap fill (BFS nearest-neighbour inpainting)
                │       → rescale + PNG/JPEG/WebP encode (render.rs)
                │       → tile cache write
                │
                └── POST /{name}/build
                        re-search STAC → rebuild index → clear tile cache
```

**Spatial index** — quadkey grid at `quadkey_zoom` (default 8). Each scene is indexed against its own STAC bbox, clipped to the tileset extent. Scenes are sorted by cloud cover ascending within each cell. At render time, scenes whose STAC bbox does not overlap the requested tile's WGS84 footprint are skipped before any COG reads, eliminating false-positive hits from adjacent MGRS tiles that share a quadkey cell.

**COG reading** — `async-tiff` reads IFDs and pixel windows directly from S3 over HTTP, with one shared `object_store` connection per hostname (avoids redundant TLS handshakes). IFDs are cached per URL for the server lifetime. Overview selection allows up to 2x upsampling to reduce fetched tile count at low zoom levels.

**Warp** — per-scene, a 256×256 UTM→WebMercator coordinate grid is precomputed once, then reused for all band warps (bilinear) and the SCL warp (nearest-neighbour). This amortizes the proj4rs transform cost across bands.

**Compositing** — `best_pixel` picks the lowest-cloud-cover valid pixel across scenes. When `scl_masking: true` (default), valid SCL classes are 4 (vegetation), 5 (bare soil), 6 (water), 7 (low-probability cloud); an optional `haze_dn_max` threshold further rejects pixels where all bands exceed the value, catching thin haze that passes SCL validation. When `scl_masking: false`, any non-zero pixel is accepted without cloud filtering — suited for arid or urban areas where SCL misclassifies bright surfaces. Remaining gaps after compositing are filled by nearest-neighbour inpainting constrained to the scene footprint. `median` is expensive for large extents.

---

## Known limitations

- Tile size is hardcoded to 256px.
- Tile cache has no TTL. The `memory` and `duckdb` backends grow indefinitely until `POST /{name}/build`. Use `none` or manage externally for near-realtime data.
- `median` composite reads all scenes regardless of fill; avoid for large extents or many scenes.
- No authentication. COGs are fetched from public S3 (Element84 Earth Search). For Microsoft Planetary Computer, assets require signing via their token API before use.
- Large extents with many years (e.g. Europe × 5 years) can produce DuckDB index files of 50–200MB; this is fine but loading is slower than small configs.
