// The Pirate Bay API (apibay.org) - catches TPB uploads Knaben hasn't indexed
use crate::sources::{http_client, looks_like_switch, Hit, SEARCH_QUERIES};
use reqwest::Client;
use serde::Deserialize;
use tracing::warn;

const API_URL: &str = "https://apibay.org/q.php";
// "No results" comes back as a single entry with this hash
const NO_RESULTS_HASH: &str = "0000000000000000000000000000000000000000";

// apibay sends every number as a string
#[derive(Debug, Deserialize)]
struct ApibayHit {
    name: String,
    info_hash: String,
    seeders: String,
    size: String,
    category: String,
}

#[derive(Clone)]
pub struct ApibayClient {
    client: Client,
}

impl ApibayClient {
    pub fn new() -> Self {
        Self { client: http_client() }
    }

    pub async fn fetch_switch(&self) -> Result<Vec<Hit>, reqwest::Error> {
        let mut hits = Vec::new();

        for query in SEARCH_QUERIES {
            let found: Vec<ApibayHit> = match self.search(query).await {
                Ok(found) => found,
                Err(e) => {
                    warn!("apibay search for {:?} failed: {}", query, e);
                    continue;
                }
            };

            hits.extend(found.into_iter()
                // 5xx categories are porn; "NSP-000" is a JAV studio code
                .filter(|h| h.info_hash != NO_RESULTS_HASH && !h.category.starts_with('5'))
                .filter(|h| looks_like_switch(&h.name, &[]))
                .map(|h| Hit {
                    hash: h.info_hash.to_uppercase(),
                    size: h.size.parse().unwrap_or(0),
                    seeders: h.seeders.parse().unwrap_or(0),
                    title: h.name,
                    magnet: None,
                }));
        }

        Ok(hits)
    }

    async fn search(&self, query: &str) -> Result<Vec<ApibayHit>, reqwest::Error> {
        self.client.get(API_URL)
            .query(&[("q", query), ("cat", "0")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }
}
