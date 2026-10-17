// torrents-csv.com - an open index of torrents seen on the DHT
use crate::sources::{http_client, looks_like_switch, Hit, SEARCH_QUERIES};
use reqwest::Client;
use serde::Deserialize;
use tracing::warn;

const API_URL: &str = "https://torrents-csv.com/service/search";
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 50;

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    torrents: Vec<CsvTorrent>,
    next: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CsvTorrent {
    infohash: String,
    name: String,
    size_bytes: Option<u64>,
    seeders: Option<u64>,
}

#[derive(Clone)]
pub struct TorrentsCsvClient {
    client: Client,
}

impl TorrentsCsvClient {
    pub fn new() -> Self {
        Self { client: http_client() }
    }

    pub async fn fetch_switch(&self) -> Result<Vec<Hit>, reqwest::Error> {
        let mut hits = Vec::new();

        for query in SEARCH_QUERIES {
            if let Err(e) = self.search_into(query, &mut hits).await {
                warn!("torrents-csv search for {:?} failed: {}", query, e);
            }
        }

        Ok(hits)
    }

    async fn search_into(&self, query: &str, hits: &mut Vec<Hit>) -> Result<(), reqwest::Error> {
        let mut after: Option<u64> = None;

        for _ in 0..MAX_PAGES {
            let mut request = self.client.get(API_URL)
                .query(&[("q", query), ("size", &PAGE_SIZE.to_string())]);
            if let Some(after) = after {
                request = request.query(&[("after", after)]);
            }

            let page: SearchResponse = request.send().await?.error_for_status()?.json().await?;
            let page_len = page.torrents.len();

            hits.extend(page.torrents.into_iter()
                .filter(|t| looks_like_switch(&t.name, &[]))
                .map(|t| Hit {
                    hash: t.infohash.to_uppercase(),
                    size: t.size_bytes.unwrap_or(0),
                    seeders: t.seeders.unwrap_or(0),
                    title: t.name,
                    magnet: None,
                }));

            match page.next {
                Some(next) if page_len > 0 => after = Some(next),
                _ => break,
            }
        }

        Ok(())
    }
}
