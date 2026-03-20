/// Pluggable tile cache backends.
///
/// All backends implement `TileCache` and are stored as `Arc<dyn TileCache>` in `AppState`.
/// The cache key includes the tileset name, so one cache instance serves all tilesets.
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use object_store::{path::Path, ObjectStore, ObjectStoreExt};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tracing::warn;

// ─── Key ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub tileset: String,
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub format: String,
}

impl TileKey {
    pub fn new(tileset: impl Into<String>, z: u8, x: u32, y: u32, format: impl Into<String>) -> Self {
        Self { tileset: tileset.into(), z, x, y, format: format.into() }
    }

    /// Canonical path: `{tileset}/{z}/{x}/{y}.{format}`
    pub fn to_path(&self) -> String {
        format!("{}/{}/{}/{}.{}", self.tileset, self.z, self.x, self.y, self.format)
    }
}

// ─── Trait ───────────────────────────────────────────────────────────────────

#[async_trait]
pub trait TileCache: Send + Sync + 'static {
    async fn get(&self, key: &TileKey) -> Result<Option<Bytes>>;
    async fn put(&self, key: &TileKey, data: Bytes) -> Result<()>;
    /// Remove all cached tiles for a tileset (called on POST /{name}/build).
    async fn clear_tileset(&self, tileset: &str) -> Result<()>;
}

// ─── None ────────────────────────────────────────────────────────────────────

pub struct NoCache;

#[async_trait]
impl TileCache for NoCache {
    async fn get(&self, _key: &TileKey) -> Result<Option<Bytes>> { Ok(None) }
    async fn put(&self, _key: &TileKey, _data: Bytes) -> Result<()> { Ok(()) }
    async fn clear_tileset(&self, _tileset: &str) -> Result<()> { Ok(()) }
}

// ─── Memory ──────────────────────────────────────────────────────────────────

pub struct MemoryCache {
    inner: DashMap<TileKey, Bytes>,
}

impl MemoryCache {
    pub fn new() -> Self {
        Self { inner: DashMap::new() }
    }
}

#[async_trait]
impl TileCache for MemoryCache {
    async fn get(&self, key: &TileKey) -> Result<Option<Bytes>> {
        Ok(self.inner.get(key).map(|v| v.clone()))
    }

    async fn put(&self, key: &TileKey, data: Bytes) -> Result<()> {
        self.inner.insert(key.clone(), data);
        Ok(())
    }

    async fn clear_tileset(&self, tileset: &str) -> Result<()> {
        self.inner.retain(|k, _| k.tileset != tileset);
        Ok(())
    }
}

// ─── Local filesystem ────────────────────────────────────────────────────────

pub struct LocalCache {
    root: PathBuf,
}

impl LocalCache {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { root: path.into() }
    }

    fn tile_path(&self, key: &TileKey) -> PathBuf {
        self.root
            .join(&key.tileset)
            .join(key.z.to_string())
            .join(key.x.to_string())
            .join(format!("{}.{}", key.y, key.format))
    }
}

#[async_trait]
impl TileCache for LocalCache {
    async fn get(&self, key: &TileKey) -> Result<Option<Bytes>> {
        let path = self.tile_path(key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(Bytes::from(bytes))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn put(&self, key: &TileKey, data: Bytes) -> Result<()> {
        let path = self.tile_path(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, &data).await?;
        Ok(())
    }

    async fn clear_tileset(&self, tileset: &str) -> Result<()> {
        let dir = self.root.join(tileset);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

// ─── DuckDB ──────────────────────────────────────────────────────────────────

pub struct DuckDbCache {
    conn: Arc<Mutex<duckdb::Connection>>,
}

impl DuckDbCache {
    pub fn new(path: &str) -> Result<Self> {
        let conn = duckdb::Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tile_cache (
                tileset TEXT     NOT NULL,
                z       INTEGER  NOT NULL,
                x       INTEGER  NOT NULL,
                y       INTEGER  NOT NULL,
                format  TEXT     NOT NULL,
                data    BLOB     NOT NULL,
                PRIMARY KEY (tileset, z, x, y, format)
            );",
        )?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }
}

#[async_trait]
impl TileCache for DuckDbCache {
    async fn get(&self, key: &TileKey) -> Result<Option<Bytes>> {
        let conn = self.conn.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<Bytes>> {
            let conn = conn.lock().map_err(|_| anyhow::anyhow!("mutex poisoned"))?;
            match conn.query_row(
                "SELECT data FROM tile_cache
                 WHERE tileset=? AND z=? AND x=? AND y=? AND format=?",
                duckdb::params![
                    key.tileset.as_str(),
                    key.z as i32,
                    key.x as i32,
                    key.y as i32,
                    key.format.as_str()
                ],
                |row| row.get::<_, Vec<u8>>(0),
            ) {
                Ok(bytes) => Ok(Some(Bytes::from(bytes))),
                Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }

    async fn put(&self, key: &TileKey, data: Bytes) -> Result<()> {
        let conn = self.conn.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().map_err(|_| anyhow::anyhow!("mutex poisoned"))?;
            conn.execute(
                "INSERT OR REPLACE INTO tile_cache (tileset, z, x, y, format, data)
                 VALUES (?, ?, ?, ?, ?, ?)",
                duckdb::params![
                    key.tileset.as_str(),
                    key.z as i32,
                    key.x as i32,
                    key.y as i32,
                    key.format.as_str(),
                    data.to_vec()
                ],
            )?;
            Ok(())
        })
        .await?
    }

    async fn clear_tileset(&self, tileset: &str) -> Result<()> {
        let conn = self.conn.clone();
        let tileset = tileset.to_string();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().map_err(|_| anyhow::anyhow!("mutex poisoned"))?;
            conn.execute(
                "DELETE FROM tile_cache WHERE tileset=?",
                duckdb::params![tileset.as_str()],
            )?;
            Ok(())
        })
        .await?
    }
}

// ─── Object store (S3 / R2) ──────────────────────────────────────────────────
//
// Generic over the concrete store type to avoid `dyn ObjectStore`, which is not
// object-safe in object_store 0.13 (native async fn in traits).

pub struct ObjectStoreCache<S> {
    store: S,
    prefix: String,
}

impl<S: ObjectStore> ObjectStoreCache<S> {
    pub fn new(store: S, prefix: impl Into<String>) -> Self {
        Self { store, prefix: prefix.into() }
    }

    fn tile_path(&self, key: &TileKey) -> Path {
        let s = if self.prefix.is_empty() {
            key.to_path()
        } else {
            format!("{}/{}", self.prefix, key.to_path())
        };
        Path::from(s.as_str())
    }

    fn tileset_prefix(&self, tileset: &str) -> Path {
        let s = if self.prefix.is_empty() {
            tileset.to_string()
        } else {
            format!("{}/{}", self.prefix, tileset)
        };
        Path::from(s.as_str())
    }
}

#[async_trait]
impl<S: ObjectStore + 'static> TileCache for ObjectStoreCache<S> {
    async fn get(&self, key: &TileKey) -> Result<Option<Bytes>> {
        let path = self.tile_path(key);
        match self.store.get(&path).await {
            Ok(result) => Ok(Some(result.bytes().await?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn put(&self, key: &TileKey, data: Bytes) -> Result<()> {
        let path = self.tile_path(key);
        self.store.put(&path, object_store::PutPayload::from(data)).await?;
        Ok(())
    }

    async fn clear_tileset(&self, tileset: &str) -> Result<()> {
        use futures::TryStreamExt;

        let prefix = self.tileset_prefix(tileset);
        let paths: Vec<Path> = self
            .store
            .list(Some(&prefix))
            .map_ok(|meta| meta.location)
            .try_collect()
            .await?;

        if paths.is_empty() {
            return Ok(());
        }

        let stream = futures::stream::iter(
            paths.into_iter().map(|p| Ok::<_, object_store::Error>(p)),
        );
        self.store
            .delete_stream(Box::pin(stream))
            .try_collect::<Vec<_>>()
            .await
            .unwrap_or_else(|e| {
                warn!("Some tiles could not be deleted from object store: {e}");
                vec![]
            });

        Ok(())
    }
}
