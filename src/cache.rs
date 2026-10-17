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

/// A Switch file inside a torrent, as reported by TorBox
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TorrentFile {
    pub hash: String,
    pub name: String,
    pub size: u64,
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
    pub async fn set_files(&self, hash: &str, files: &[(String, u64)]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM files WHERE hash = ?")
            .bind(hash)
            .execute(&mut *tx)
            .await?;

        for (name, size) in files {
            sqlx::query("INSERT OR IGNORE INTO files (hash, name, size) VALUES (?, ?, ?)")
                .bind(hash)
                .bind(name)
                .bind(*size as i64)
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
    pub async fn get_all_files(&self) -> Result<Vec<TorrentFile>, sqlx::Error> {
        let rows = sqlx::query("SELECT hash, name, size FROM files ORDER BY hash, name")
            .fetch_all(&self.pool)
            .await?;

        rows.iter()
            .map(|row| Ok(TorrentFile {
                hash: row.try_get("hash")?,
                name: row.try_get("name")?,
                size: row.try_get::<i64, _>("size")? as u64,
            }))
            .collect()
    }

    pub async fn get_files(&self, hash: &str) -> Result<Vec<TorrentFile>, sqlx::Error> {
        let rows = sqlx::query("SELECT hash, name, size FROM files WHERE hash = ? ORDER BY name")
            .bind(hash.to_uppercase())
            .fetch_all(&self.pool)
            .await?;

        rows.iter()
            .map(|row| Ok(TorrentFile {
                hash: row.try_get("hash")?,
                name: row.try_get("name")?,
                size: row.try_get::<i64, _>("size")? as u64,
            }))
            .collect()
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
