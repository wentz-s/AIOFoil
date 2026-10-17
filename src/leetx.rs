// 1337x, through FlareSolverr (1337x sits behind Cloudflare's bot check).
//
// FlareSolverr runs a real browser that solves the check. Start it with:
//   docker run -d --name flaresolverr -p 127.0.0.1:8191:8191 ghcr.io/flaresolverr/flaresolverr:latest
//
// Search pages list name, seeders and size but no info hash, so each torrent's
// own page is opened once to get it. Hashes are kept in the cache database, so
// later refreshes only open pages for new torrents.
use crate::cache::Cache;
use crate::sources::{html_unescape, http_client, looks_like_switch, parse_size, Error, Hit};
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use std::sync::LazyLock;
use tracing::{info, warn};

const BASE_URL: &str = "https://1337x.to";
// 1337x's own "Switch" category
const SWITCH_CATEGORY: &str = "/sub/82";
// Searches in the Games category catch Switch releases filed elsewhere
const GAME_SEARCHES: [&str; 3] = ["nsp", "nsz", "xci"];
const MAX_PAGES: usize = 100;

static ROW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<tr>(.*?)</tr>").unwrap());
static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<a href="(/torrent/[^"]+)">([^<]*)</a>"#).unwrap());
static SEEDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<td class="coll-2 seeds">(\d+)</td>"#).unwrap());
static SIZE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<td class="coll-4 size[^"]*">([^<]+)<"#).unwrap());
static INFO_HASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"btih:([0-9a-fA-F]{40})").unwrap());

#[derive(Debug, Deserialize)]
struct FlareResponse {
    status: String,
    #[serde(default)]
    message: String,
    solution: Option<FlareSolution>,
}

#[derive(Debug, Deserialize)]
struct FlareSolution {
    status: u16,
    // The page HTML
    #[serde(default)]
    response: String,
}

/// One row of a 1337x search page
struct Listing {
    path: String,
    title: String,
    seeders: u64,
    size: u64,
}

#[derive(Clone)]
pub struct LeetxClient {
    client: Client,
    flaresolverr_url: String,
    cache: Cache,
    // Per process, so two running servers never close each other's session
    session: String,
}

impl LeetxClient {
    pub fn new(flaresolverr_url: String, cache: Cache) -> Self {
        Self {
            client: http_client(),
            flaresolverr_url,
            cache,
            session: format!("{}-{}", env!("CARGO_PKG_NAME"), std::process::id()),
        }
    }

    pub async fn fetch_switch(&self) -> Result<Vec<Hit>, Error> {
        // One browser session for the whole crawl, so Cloudflare is solved once
        self.flare(serde_json::json!({ "cmd": "sessions.create", "session": self.session })).await
            .map_err(|e| format!("FlareSolverr unavailable at {}: {e}", self.flaresolverr_url))?;

        let result = self.crawl().await;

        if let Err(e) = self.flare(serde_json::json!({ "cmd": "sessions.destroy", "session": self.session })).await {
            warn!("Couldn't close FlareSolverr session: {}", e);
        }
        result
    }

    async fn crawl(&self) -> Result<Vec<Hit>, Error> {
        let mut listings = self.crawl_pages(SWITCH_CATEGORY, |page| format!("{SWITCH_CATEGORY}/{page}/")).await?;

        for query in GAME_SEARCHES {
            let found = self.crawl_pages(query, |page| format!("/category-search/{query}/Games/{page}/")).await?;
            listings.extend(found.into_iter().filter(|l| looks_like_switch(&l.title, &[])));
        }

        listings.sort_by(|a, b| a.path.cmp(&b.path));
        listings.dedup_by(|a, b| a.path == b.path);

        // Info hashes: from the cache where we've seen the page before, otherwise open it
        let known = self.cache.get_page_hashes().await?;
        let new_pages = listings.iter().filter(|l| !known.contains_key(&l.path)).count();
        if new_pages > 0 {
            info!("1337x: opening {} new torrent pages for their info hashes", new_pages);
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
                        warn!("1337x: couldn't open {}: {}", listing.path, e);
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
    async fn crawl_pages(&self, label: &str, page_path: impl Fn(usize) -> String) -> Result<Vec<Listing>, Error> {
        let mut listings = Vec::new();
        for page in 1..=MAX_PAGES {
            let html = match self.get(&page_path(page)).await {
                Ok(html) => html,
                Err(e) => {
                    warn!("1337x: {} page {} failed: {}", label, page, e);
                    break;
                }
            };
            let found = parse_listings(&html);
            if found.is_empty() {
                break;
            }
            listings.extend(found);
        }
        Ok(listings)
    }

    async fn fetch_hash(&self, path: &str) -> Result<Option<String>, Error> {
        let html = self.get(path).await?;
        Ok(INFO_HASH.captures(&html).map(|c| c[1].to_uppercase()))
    }

    async fn get(&self, path: &str) -> Result<String, Error> {
        let response = self.flare(serde_json::json!({
            "cmd": "request.get",
            "url": format!("{BASE_URL}{path}"),
            "session": self.session,
            "maxTimeout": 60_000,
            // Without this, torrent pages take ~30 s waiting on ads
            "disableMedia": true,
        })).await?;

        let solution = response.solution.ok_or("FlareSolverr returned no page")?;
        if solution.status != 200 {
            return Err(format!("HTTP {}", solution.status).into());
        }
        Ok(solution.response)
    }

    async fn flare(&self, body: serde_json::Value) -> Result<FlareResponse, Error> {
        let response: FlareResponse = self.client.post(&self.flaresolverr_url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;

        if response.status != "ok" {
            return Err(response.message.into());
        }
        Ok(response)
    }
}

fn parse_listings(html: &str) -> Vec<Listing> {
    ROW.captures_iter(html)
        .filter_map(|row| {
            let row = &row[1];
            let name = NAME.captures(row)?;
            Some(Listing {
                path: name[1].to_string(),
                title: html_unescape(name[2].trim()),
                seeders: SEEDS.captures(row).and_then(|c| c[1].parse().ok()).unwrap_or(0),
                size: SIZE.captures(row).map(|c| parse_size(&c[1])).unwrap_or(0),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_rows() {
        let html = r#"<table><tr><th>name</th></tr><tr>
            <td class="coll-1 name"><a href="/sub/82/0/" class="icon"><i class="flaticon-psp"></i></a><a href="/torrent/4359521/Animal-Crossing-New-Horizons-NSP-XCI/">Animal Crossing New Horizons [NSP + XCI]</a></td>
            <td class="coll-2 seeds">151</td>
            <td class="coll-3 leeches">180</td>
            <td class="coll-4 size mob-user">12.8 GB<span class="seeds">151</span></td>
            </tr><tr>
            <td class="coll-1 name"><a href="/torrent/1/Mario-Luigi/">Mario &amp; Luigi: Brothership [SWITCH]</a></td>
            <td class="coll-2 seeds">7</td>
            <td class="coll-4 size mob-vip">512.5 MB<span class="seeds">7</span></td>
            </tr></table>"#;

        let rows = parse_listings(html);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, "/torrent/4359521/Animal-Crossing-New-Horizons-NSP-XCI/");
        assert_eq!(rows[0].title, "Animal Crossing New Horizons [NSP + XCI]");
        assert_eq!(rows[0].seeders, 151);
        assert_eq!(rows[0].size, (12.8 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(rows[1].title, "Mario & Luigi: Brothership [SWITCH]");
    }
}
