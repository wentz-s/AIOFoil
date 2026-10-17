// TorrentGalaxy (torrentgalaxy.one) - plain HTTP, no FlareSolverr needed.
//
// Search pages list title, size and seeders but no info hash, so each
// torrent's detail page is opened once to get it. Hashes are kept in the
// cache database, so later refreshes only open pages for new torrents.
use crate::cache::Cache;
use crate::sources::{html_unescape, http_client, looks_like_switch, parse_size, Error, Hit};
use regex::Regex;
use reqwest::Client;
use std::sync::LazyLock;
use tracing::{info, warn};

const BASE_URL: &str = "https://torrentgalaxy.one";
// Searched within the Games category
const QUERIES: [&str; 4] = ["switch", "nsp", "nsz", "xci"];
const MAX_PAGES: usize = 50;

static NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<a class="txlight" title="[^"]*"\s+href="(/post-detail/[^"]+)">.*?<b>([^<]+)</b>"#).unwrap()
});
static SIZE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)<span\s+class="badge badge-secondary txlight"[^>]*>([^<]+)</span>"#).unwrap());
static SEEDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)title="Seeders/Leechers">\[<font\s+color="green"><b>(\d+)</b>"#).unwrap());
// The detail page also links related torrents, so only trust the labelled hash
static INFO_HASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Info Hash:\s*(?:<[^>]+>\s*)*([0-9a-fA-F]{40})").unwrap());

/// One row of a TorrentGalaxy search page
struct Listing {
    path: String,
    title: String,
    seeders: u64,
    size: u64,
}

#[derive(Clone)]
pub struct TgxClient {
    client: Client,
    cache: Cache,
}

impl TgxClient {
    pub fn new(cache: Cache) -> Self {
        Self { client: http_client(), cache }
    }

    pub async fn fetch_switch(&self) -> Result<Vec<Hit>, Error> {
        let mut listings = Vec::new();
        for query in QUERIES {
            listings.extend(self.search(query).await.into_iter().filter(|l| looks_like_switch(&l.title, &[])));
        }

        listings.sort_by(|a, b| a.path.cmp(&b.path));
        listings.dedup_by(|a, b| a.path == b.path);

        // Info hashes: from the cache where we've seen the page before, otherwise open it
        let known = self.cache.get_page_hashes().await?;
        let new_pages = listings.iter().filter(|l| !known.contains_key(&l.path)).count();
        if new_pages > 0 {
            info!("TorrentGalaxy: opening {} new torrent pages for their info hashes", new_pages);
        }

        let mut hits = Vec::new();
        for listing in listings {
            let hash = match known.get(&listing.path) {
                Some(hash) => hash.clone(),
                None => match self.fetch_hash(&listing.path).await {
                    Ok(Some(hash)) => {
                        self.cache.set_page_hash(&listing.path, &hash).await?;
                        hash
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        warn!("TorrentGalaxy: couldn't open {}: {}", listing.path, e);
                        continue;
                    }
                },
            };
            hits.push(Hit {
                title: listing.title,
                hash,
                size: listing.size,
                seeders: listing.seeders,
                magnet: None,
            });
        }

        Ok(hits)
    }

    /// Read result pages until one comes back empty.
    async fn search(&self, query: &str) -> Vec<Listing> {
        let mut listings = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = format!("{BASE_URL}/get-posts/keywords:{query}:category:Games/?page={page}");
            let html = match self.get(&url).await {
                Ok(html) => html,
                Err(e) => {
                    warn!("TorrentGalaxy: {:?} page {} failed: {}", query, page, e);
                    break;
                }
            };
            let found = parse_listings(&html);
            if found.is_empty() {
                break;
            }
            listings.extend(found);
        }
        listings
    }

    async fn fetch_hash(&self, path: &str) -> Result<Option<String>, Error> {
        let html = self.get(&format!("{BASE_URL}{path}")).await?;
        Ok(INFO_HASH.captures(&html).map(|c| c[1].to_uppercase()))
    }

    async fn get(&self, url: &str) -> Result<String, reqwest::Error> {
        self.client.get(url).send().await?.error_for_status()?.text().await
    }
}

fn parse_listings(html: &str) -> Vec<Listing> {
    html.split(r#"class="tgxtablerow"#)
        .skip(1)
        .filter_map(|row| {
            let name = NAME.captures(row)?;
            Some(Listing {
                path: name[1].to_string(),
                title: html_unescape(name[2].trim()),
                seeders: SEEDS.captures(row).and_then(|c| c[1].parse().ok()).unwrap_or(0),
                size: SIZE.captures(row).map(|c| parse_size(c[1].trim())).unwrap_or(0),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_rows() {
        let html = r##"<div class="tgxtable"><div class="tgxtablerow txlight" onmouseover="...">
            <div class="tgxtablecell clickable-row click textshadow rounded txlight" id="click"
            data-href="/post-detail/8701cb/switch-nsp-hogwarts-legacy/">
            <div><a class="txlight" title="[Switch NSP] Hogwarts Legacy"
            href="/post-detail/8701cb/switch-nsp-hogwarts-legacy/"><span
            src="torrent"><b>[Switch NSP] Hogwarts Legacy</b></span></a></div></div>
            <div class="tgxtablecell collapsehide rounded txlight" style="text-align:right;"><span
            class="badge badge-secondary txlight"
            style="border-radius:4px;">17.1 GB</span></div>
            <div class="tgxtablecell collapsehide rounded txlight"><span title="Seeders/Leechers">[<font
            color="green"><b>14</b></font>/<font color="#ff0000"><b>3</b></font>]</span></div>
            </div>"##;

        let rows = parse_listings(html);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "/post-detail/8701cb/switch-nsp-hogwarts-legacy/");
        assert_eq!(rows[0].title, "[Switch NSP] Hogwarts Legacy");
        assert_eq!(rows[0].seeders, 14);
        assert_eq!(rows[0].size, (17.1 * 1024.0 * 1024.0 * 1024.0) as u64);
    }

    #[test]
    fn takes_the_labelled_hash() {
        let html = r#"related btih:97D6A5BB8755F11AF9B3F3397A853818E8545AEF
            Total Size: 17.1 GB <div>Info Hash: <b>504A375F83377EEFC8F5BDF1DBD87ABF1A7B4E04</b></div>"#;
        assert_eq!(
            INFO_HASH.captures(html).map(|c| c[1].to_string()).as_deref(),
            Some("504A375F83377EEFC8F5BDF1DBD87ABF1A7B4E04")
        );
    }
}
