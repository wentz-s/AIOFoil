// Aggregator - pulls Switch torrents from every enabled source into the cache,
// tags them with title IDs, and fills in torrent file lists from TorBox
use crate::cache::{Cache, Game, StoredFile};
use crate::config::Config;
use crate::apibay::ApibayClient;
use crate::knaben::KnabenClient;
use crate::leetx::LeetxClient;
use crate::nyaa::NyaaClient;
use crate::rar::is_rar_volume;
use crate::sources::{is_archive_title, magnet_for, Error, Hit, Source};
use crate::tgx::TgxClient;
use crate::torrents_csv::TorrentsCsvClient;
use crate::titledb::{title_id_in, TitleDb};
use crate::torbox::{base_name, is_switch_file, is_zip_file, TorBoxClient};
use std::collections::{HashMap, HashSet};
use tracing::{error, info, warn};

// How many hashes to send per TorBox checkcached request
const CHECK_CACHED_BATCH: usize = 100;

#[derive(Clone)]
pub struct Aggregator {
    config: Config,
    cache: Cache,
    knaben_client: KnabenClient,
    apibay_client: ApibayClient,
    torrents_csv_client: TorrentsCsvClient,
    nyaa_client: NyaaClient,
    leetx_client: LeetxClient,
    tgx_client: TgxClient,
    torbox_client: Option<TorBoxClient>,
}

impl Aggregator {
    pub fn new(config: &Config, cache: &Cache) -> Self {
        Self {
            config: config.clone(),
            cache: cache.clone(),
            knaben_client: KnabenClient::new(config.knaben_url.clone()),
            apibay_client: ApibayClient::new(),
            torrents_csv_client: TorrentsCsvClient::new(),
            nyaa_client: NyaaClient::new(),
            leetx_client: LeetxClient::new(config.flaresolverr_url.clone(), cache.clone()),
            tgx_client: TgxClient::new(cache.clone()),
            torbox_client: config.torbox_api_key.clone().map(TorBoxClient::new),
        }
    }

    pub async fn refresh(&self, titledb: Option<&TitleDb>) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let now = chrono::Utc::now().timestamp();

        // Merge every source by hash, keeping the highest seeder count seen
        let mut merged: HashMap<String, Hit> = HashMap::new();
        for source in &self.config.sources {
            let hits = match source {
                Source::Knaben => self.knaben_client.fetch_switch(self.config.knaben_category).await.map_err(Error::from),
                Source::Apibay => self.apibay_client.fetch_switch().await.map_err(Error::from),
                Source::TorrentsCsv => self.torrents_csv_client.fetch_switch().await.map_err(Error::from),
                Source::Nyaa => self.nyaa_client.fetch_switch().await,
                Source::Leetx => self.leetx_client.fetch_switch().await,
                Source::Tgx => self.tgx_client.fetch_switch().await,
            };
            let hits = match hits {
                Ok(hits) => hits,
                Err(e) => {
                    warn!("{:?} failed: {}", source, e);
                    continue;
                }
            };

            let before = merged.len();
            for hit in hits {
                match merged.get_mut(&hit.hash) {
                    Some(existing) => {
                        existing.seeders = existing.seeders.max(hit.seeders);
                        if existing.magnet.is_none() {
                            existing.magnet = hit.magnet;
                        }
                    }
                    None => {
                        merged.insert(hit.hash.clone(), hit);
                    }
                }
            }
            info!("{:?}: {} new torrents", source, merged.len() - before);
        }

        if merged.is_empty() {
            return Err("every source failed or returned nothing".into());
        }

        let (archives, hits): (Vec<Hit>, Vec<Hit>) = merged.into_values()
            .partition(|hit| is_archive_title(&hit.title));
        for hit in &archives {
            // In case an earlier refresh stored it
            self.cache.mark_unusable(&hit.hash).await?;
        }
        if !archives.is_empty() {
            info!("Skipped {} torrents that are 7z archives", archives.len());
        }

        let games: Vec<Game> = hits.into_iter()
            .filter(|hit| hit.seeders >= self.config.min_seeders)
            .map(|hit| {
                // An ID in the torrent name beats a name match
                let title_id = title_id_in(&hit.title)
                    .or_else(|| titledb.and_then(|db| db.lookup(&hit.title)))
                    .map(str::to_uppercase);
                let magnet = hit.magnet.unwrap_or_else(|| magnet_for(&hit.hash, &hit.title));
                Game {
                    hash: hit.hash,
                    title: hit.title,
                    size: hit.size,
                    seeders: hit.seeders,
                    magnet,
                    title_id,
                    updated_at: now,
                }
            })
            .collect();

        self.cache.upsert_games(&games).await?;

        let matched = games.iter().filter(|g| g.title_id.is_some()).count();
        info!("{} torrents with at least {} seeder(s), {} with a title ID", games.len(), self.config.min_seeders, matched);

        // Drop torrents that have been missing from every source for a few refreshes
        let stale_after = 3 * self.config.refresh_hours as i64 * 3600;
        let removed = self.cache.clear_older_than(now - stale_after).await?;
        if removed > 0 {
            info!("Removed {} stale games", removed);
        }

        if let Some(torbox) = &self.torbox_client {
            if let Err(e) = self.fetch_file_lists(torbox, &games).await {
                warn!("Couldn't get file lists from TorBox: {}", e);
            }
        }

        Ok(games.len())
    }

    /// Ask TorBox for the file lists of any torrents it has cached that we
    /// don't have a file list for yet.
    async fn fetch_file_lists(&self, torbox: &TorBoxClient, games: &[Game]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let known: HashSet<String> = self.cache.hashes_with_files().await?.into_iter().collect();
        let missing: Vec<String> = games.iter()
            .map(|g| g.hash.clone())
            .filter(|h| !known.contains(h))
            .collect();

        let (mut found, mut unusable) = (0, 0);
        for batch in missing.chunks(CHECK_CACHED_BATCH) {
            for torrent in torbox.check_cached(batch).await? {
                let hash = torrent.hash.to_uppercase();
                let files: Vec<StoredFile> = torrent.files.iter()
                    .filter(|f| is_switch_file(&f.name))
                    .map(|f| StoredFile {
                        hash: hash.clone(),
                        name: base_name(&f.name).to_string(),
                        size: f.size,
                        archive: None,
                        header_offset: None,
                        parts: None,
                    })
                    .collect();
                let has_archive = torrent.files.iter().any(|f| is_zip_file(&f.name) || is_rar_volume(&f.name));
                if !files.is_empty() {
                    self.cache.set_files(&hash, &files).await?;
                    found += 1;
                } else if !torrent.files.is_empty() && !has_archive {
                    // Files, but nothing Tinfoil can install: 7z archives, or not a Switch game.
                    // ZIPs and RARs are looked inside on first download instead.
                    self.cache.mark_unusable(&hash).await?;
                    unusable += 1;
                }
            }
        }

        info!("Got file lists for {} more torrents from TorBox, hid {} with no installable files", found, unusable);
        Ok(())
    }

    /// Refresh now, then every `refresh_hours`, forever.
    pub async fn run(self) {
        let titledb = match TitleDb::load(&self.config.cache_dir()).await {
            Ok(db) => Some(db),
            Err(e) => {
                warn!("titledb unavailable, games will only get title IDs from file names: {}", e);
                None
            }
        };

        let mut interval = tokio::time::interval(
            std::time::Duration::from_secs(self.config.refresh_hours.max(1) * 3600),
        );
        loop {
            interval.tick().await;
            info!("Refreshing game list...");
            match self.refresh(titledb.as_ref()).await {
                Ok(count) => info!("Cached {} games", count),
                Err(e) => error!("Refresh failed: {}", e),
            }
        }
    }
}
