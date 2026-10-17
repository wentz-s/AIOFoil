// Nyaa (nyaa.si) RSS feed, "Software - Games" category. Good for Japanese
// releases. The feed includes info hashes and seeder counts.
use crate::sources::{html_unescape, http_client, looks_like_switch, parse_size, Error, Hit};
use regex::Regex;
use reqwest::Client;
use std::sync::LazyLock;
use tracing::warn;

const RSS_URL: &str = "https://nyaa.si/";
// Software - Games
const CATEGORY: &str = "6_2";
const QUERIES: [&str; 4] = ["nsp", "nsz", "xci", "switch"];

static ITEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<item>(.*?)</item>").unwrap());
static TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<title>(.*?)</title>").unwrap());
static INFO_HASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<nyaa:infoHash>([0-9a-fA-F]{40})</nyaa:infoHash>").unwrap());
static SEEDERS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<nyaa:seeders>(\d+)</nyaa:seeders>").unwrap());
static SIZE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<nyaa:size>([^<]+)</nyaa:size>").unwrap());

#[derive(Clone)]
pub struct NyaaClient {
    client: Client,
}

impl NyaaClient {
    pub fn new() -> Self {
        Self { client: http_client() }
    }

    pub async fn fetch_switch(&self) -> Result<Vec<Hit>, Error> {
        let mut hits = Vec::new();

        for query in QUERIES {
            match self.search(query).await {
                Ok(found) => hits.extend(found.into_iter().filter(|h| looks_like_switch(&h.title, &[]))),
                Err(e) => warn!("Nyaa search for {:?} failed: {}", query, e),
            }
        }

        Ok(hits)
    }

    async fn search(&self, query: &str) -> Result<Vec<Hit>, reqwest::Error> {
        let xml = self.client.get(RSS_URL)
            .query(&[("page", "rss"), ("c", CATEGORY), ("f", "0"), ("q", query)])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;

        Ok(parse_feed(&xml))
    }
}

fn parse_feed(xml: &str) -> Vec<Hit> {
    ITEM.captures_iter(xml)
        .filter_map(|item| {
            let item = &item[1];
            Some(Hit {
                title: html_unescape(TITLE.captures(item)?[1].trim()),
                hash: INFO_HASH.captures(item)?[1].to_uppercase(),
                seeders: SEEDERS.captures(item).and_then(|c| c[1].parse().ok()).unwrap_or(0),
                size: SIZE.captures(item).map(|c| parse_size(&c[1])).unwrap_or(0),
                magnet: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rss_items() {
        let xml = r#"<rss><channel><title>Nyaa</title>
            <item>
                <title>[SWITCH] ANIMAL WELL [010020D01AD24000] [v655360] (1G+1U) [NSP]</title>
                <nyaa:seeders>1</nyaa:seeders>
                <nyaa:infoHash>a0bd014b9a1e46c31914e2422af0b2b26cf8cb12</nyaa:infoHash>
                <nyaa:size>386.6 MiB</nyaa:size>
            </item>
            <item><title>no hash here</title></item>
        </channel></rss>"#;

        let hits = parse_feed(xml);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hash, "A0BD014B9A1E46C31914E2422AF0B2B26CF8CB12");
        assert_eq!(hits[0].seeders, 1);
        assert_eq!(hits[0].size, (386.6 * 1024.0 * 1024.0) as u64);
        assert!(hits[0].title.starts_with("[SWITCH] ANIMAL WELL"));
    }
}
