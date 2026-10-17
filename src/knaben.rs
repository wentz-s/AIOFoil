// Knaben API client
use crate::sources::{http_client, looks_like_switch, Hit, SEARCH_QUERIES};
use reqwest::Client;
use serde::Deserialize;
use tracing::warn;

const PAGE_SIZE: usize = 300;
const MAX_RESULTS: usize = 10_000;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KnabenHit {
    title: String,
    hash: Option<String>,
    bytes: Option<u64>,
    seeders: Option<u64>,
    magnet_url: Option<String>,
    #[serde(default)]
    category_id: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct KnabenResponse {
    hits: Vec<KnabenHit>,
}

#[derive(Clone)]
pub struct KnabenClient {
    client: Client,
    api_url: String,
}

impl KnabenClient {
    pub fn new(api_url: String) -> Self {
        Self {
            client: http_client(),
            api_url,
        }
    }

    /// Everything in the Switch category, plus Switch releases filed elsewhere
    /// (mostly "PC / Games") found by title search.
    pub async fn fetch_switch(&self, category: u64) -> Result<Vec<Hit>, reqwest::Error> {
        let mut hits = self.fetch_all(serde_json::json!({ "categories": [category] })).await?;

        for query in SEARCH_QUERIES {
            let filter = serde_json::json!({
                "search_type": "100%",
                "search_field": "title",
                "query": query,
            });
            match self.fetch_all(filter).await {
                Ok(found) => hits.extend(found.into_iter().filter(|h| {
                    h.category_id.contains(&category) || looks_like_switch(&h.title, &h.category_id)
                })),
                Err(e) => warn!("Knaben search for {:?} failed: {}", query, e),
            }
        }

        Ok(hits.into_iter()
            .filter_map(|h| Some(Hit {
                hash: h.hash?.to_uppercase(),
                title: h.title,
                size: h.bytes.unwrap_or(0),
                seeders: h.seeders.unwrap_or(0),
                magnet: h.magnet_url,
            }))
            .collect())
    }

    async fn fetch_all(&self, filter: serde_json::Value) -> Result<Vec<KnabenHit>, reqwest::Error> {
        let mut results = Vec::new();

        for from in (0..MAX_RESULTS).step_by(PAGE_SIZE) {
            let mut body = serde_json::json!({
                "order_by": "seeders",
                "order_direction": "desc",
                "from": from,
                "size": PAGE_SIZE,
                "hide_unsafe": true,
                "hide_xxx": true
            });
            body.as_object_mut().unwrap().extend(filter.as_object().unwrap().clone());

            let response: KnabenResponse = self.client.post(&self.api_url)
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            let page_len = response.hits.len();
            results.extend(response.hits);
            if page_len < PAGE_SIZE {
                break;
            }
        }

        Ok(results)
    }
}
