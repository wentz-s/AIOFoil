// Cache module - SQLite storage for games
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteRow};
use sqlx::{Row, SqlitePool};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Game {
    pub hash: String,
    pub title: String,
    pub size: u64,
    pub seeders: u64,
    pub magnet: String,
    pub title_id: Option<String>,
    pub updated_at: i64,
}

/// An installable file in a torrent: loose, or stored inside one of its ZIPs or RARs
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StoredFile {
    pub hash: String,
    pub name: String,
    pub size: u64,
    /// The ZIP (or first RAR volume) in the torrent that holds this file, if any
    pub archive: Option<String>,
    /// ZIP only: offset of the file's local header within the ZIP
    pub header_offset: Option<u64>,
    /// RAR only: where the file's bytes are, in order, across the volumes
    pub parts: Option<Vec<Part>>,
}

/// A stretch of a file's bytes inside one archive volume
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Part {
    pub volume: String,
    pub offset: u64,
    pub len: u64,
}

#[derive(Clone)]
pub struct Cache {
    pool: SqlitePool,
}

impl Cache {
    pub async fn new(path: &std::path::Path) -> Result<Self, sqlx::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options).await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS games (
                hash TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                size INTEGER NOT NULL,
                seeders INTEGER NOT NULL,
                magnet TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            )"
        )
        .execute(&pool)
        .await?;

        // Added after the first release; these fail harmlessly if the column already exists
        sqlx::query("ALTER TABLE games ADD COLUMN title_id TEXT")
            .execute(&pool)
            .await
            .ok();
        // Set when TorBox shows the torrent has no NSP/NSZ/XCI (usually RAR or 7z archives)
        sqlx::query("ALTER TABLE games ADD COLUMN unusable INTEGER NOT NULL DEFAULT 0")
            .execute(&pool)
            .await
            .ok();

        // Info hashes looked up from torrent pages (1337x), so each page is only opened once
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS page_hashes (
                path TEXT PRIMARY KEY,
                hash TEXT NOT NULL
            )"
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS files (
                hash TEXT NOT NULL,
                name TEXT NOT NULL,
                size INTEGER NOT NULL,
                PRIMARY KEY (hash, name)
            )"
        )
        .execute(&pool)
        .await?;

        // Added with ZIP support. The first time, unhide torrents that were hidden
        // for having no loose NSP/NSZ/XCI, since ZIPs of them can now be used.
        let added_zip_support = sqlx::query("ALTER TABLE files ADD COLUMN archive TEXT")
            .execute(&pool)
            .await
            .is_ok();
        if added_zip_support {
            sqlx::query("ALTER TABLE files ADD COLUMN header_offset INTEGER")
                .execute(&pool)
                .await?;
        }
        // Added with RAR support; likewise unhide everything once
        let added_rar_support = sqlx::query("ALTER TABLE files ADD COLUMN parts TEXT")
            .execute(&pool)
            .await
            .is_ok();
        if added_zip_support || added_rar_support {
            sqlx::query("UPDATE games SET unusable = 0")
                .execute(&pool)
                .await?;
        }

        Ok(Self { pool })
    }

    /// Insert or refresh games, keyed by info hash.
    pub async fn upsert_games(&self, games: &[Game]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        for game in games {
            sqlx::query(
                "INSERT INTO games (hash, title, size, seeders, magnet, title_id, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(hash) DO UPDATE SET
                    title = excluded.title,
                    size = excluded.size,
                    seeders = excluded.seeders,
                    magnet = excluded.magnet,
                    title_id = excluded.title_id,
                    updated_at = excluded.updated_at"
            )
            .bind(&game.hash)
            .bind(&game.title)
            .bind(game.size as i64)
            .bind(game.seeders as i64)
            .bind(&game.magnet)
            .bind(&game.title_id)
            .bind(game.updated_at)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await
    }

    fn row_to_game(row: &SqliteRow) -> Result<Game, sqlx::Error> {
        Ok(Game {
            hash: row.try_get("hash")?,
            title: row.try_get("title")?,
            size: row.try_get::<i64, _>("size")? as u64,
            seeders: row.try_get::<i64, _>("seeders")? as u64,
            magnet: row.try_get("magnet")?,
            title_id: row.try_get("title_id")?,
            updated_at: row.try_get("updated_at")?,
        })
    }

    pub async fn get_game(&self, hash: &str) -> Result<Option<Game>, sqlx::Error> {
        let row = sqlx::query("SELECT * FROM games WHERE hash = ?")
            .bind(hash.to_uppercase())
            .fetch_optional(&self.pool)
            .await?;

        row.as_ref().map(Self::row_to_game).transpose()
    }

    pub async fn get_all_games(&self) -> Result<Vec<Game>, sqlx::Error> {
        let rows = sqlx::query("SELECT * FROM games WHERE unusable = 0 ORDER BY seeders DESC")
            .fetch_all(&self.pool)
            .await?;

        rows.iter().map(Self::row_to_game).collect()
    }

    /// Replace the known file list of a torrent.
    pub async fn set_files(&self, hash: &str, files: &[StoredFile]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM files WHERE hash = ?")
            .bind(hash)
            .execute(&mut *tx)
            .await?;

        for file in files {
            let parts = file.parts.as_ref().map(serde_json::to_string).transpose()
                .map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
            sqlx::query("INSERT OR IGNORE INTO files (hash, name, size, archive, header_offset, parts) VALUES (?, ?, ?, ?, ?, ?)")
                .bind(hash)
                .bind(&file.name)
                .bind(file.size as i64)
                .bind(&file.archive)
                .bind(file.header_offset.map(|o| o as i64))
                .bind(parts)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await
    }

    /// Hide a torrent that Tinfoil can't use.
    pub async fn mark_unusable(&self, hash: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE games SET unusable = 1 WHERE hash = ?")
            .bind(hash.to_uppercase())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Hashes whose contents we already know: a file list, or known to be unusable.
    pub async fn hashes_with_files(&self) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar("SELECT DISTINCT hash FROM files UNION SELECT hash FROM games WHERE unusable = 1")
            .fetch_all(&self.pool)
            .await
    }

    /// Every known file, ordered by torrent then name. The position of a file
    /// within its torrent is its index in download URLs.
    pub async fn get_all_files(&self) -> Result<Vec<StoredFile>, sqlx::Error> {
        let rows = sqlx::query("SELECT * FROM files ORDER BY hash, name")
            .fetch_all(&self.pool)
            .await?;

        rows.iter().map(Self::row_to_file).collect()
    }

    pub async fn get_files(&self, hash: &str) -> Result<Vec<StoredFile>, sqlx::Error> {
        let rows = sqlx::query("SELECT * FROM files WHERE hash = ? ORDER BY name")
            .bind(hash.to_uppercase())
            .fetch_all(&self.pool)
            .await?;

        rows.iter().map(Self::row_to_file).collect()
    }

    fn row_to_file(row: &SqliteRow) -> Result<StoredFile, sqlx::Error> {
        Ok(StoredFile {
            hash: row.try_get("hash")?,
            name: row.try_get("name")?,
            size: row.try_get::<i64, _>("size")? as u64,
            archive: row.try_get("archive")?,
            header_offset: row.try_get::<Option<i64>, _>("header_offset")?.map(|o| o as u64),
            parts: row.try_get::<Option<String>, _>("parts")?
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
        })
    }

    pub async fn get_page_hashes(&self) -> Result<std::collections::HashMap<String, String>, sqlx::Error> {
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT path, hash FROM page_hashes")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().collect())
    }

    pub async fn set_page_hash(&self, path: &str, hash: &str) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT OR REPLACE INTO page_hashes (path, hash) VALUES (?, ?)")
            .bind(path)
            .bind(hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Remove games that haven't been seen in a refresh since `cutoff` (unix seconds).
    pub async fn clear_older_than(&self, cutoff: i64) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM games WHERE updated_at < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;

        sqlx::query("DELETE FROM files WHERE hash NOT IN (SELECT hash FROM games)")
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }
}
