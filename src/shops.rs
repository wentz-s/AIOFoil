/// Other Tinfoil shops, merged into AIOFoil's index.
/// Their links are passed through as they are, so the Switch downloads
/// straight from the shop instead of through this server.
use crate::sources::{http_client, Error};
use reqwest::Url;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{info, warn};

// Shop indexes are cheap to fetch on a LAN, so keep them fresher than torrents
const REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
// Sooner after a failure, e.g. when the shop was still starting up
const RETRY_INTERVAL: Duration = Duration::from_secs(60);
// How deep to follow "directories" links to more index files
const MAX_DEPTH: usize = 3;

/// A file listed by another shop
#[derive(Clone, Debug, PartialEq)]
pub struct ShopFile {
    /// Absolute URL, with the file name as its fragment
    pub url: String,
    pub name: String,
    pub size: u64,
}

/// Latest files from every shop, in the order the shops are configured
pub type SharedShopFiles = Arc<RwLock<Vec<ShopFile>>>;

/// Fetch every shop in `path` now, then every few minutes, forever.
/// A shop that fails keeps its last good list.
pub async fn run(path: PathBuf, files: SharedShopFiles) {
    let client = http_client();
    let mut last_good: Vec<Vec<ShopFile>> = Vec::new();
    loop {
        let mut failed = false;
        match read_shops(&path) {
            Ok(shops) => {
                last_good.resize(shops.len(), Vec::new());
                for (i, shop) in shops.iter().enumerate() {
                    match fetch_shop(&client, shop).await {
                        Ok(shop_files) => {
                            info!("Shop {}: {} files", shop, shop_files.len());
                            last_good[i] = shop_files;
                        }
                        Err(e) => {
                            warn!("Shop {} failed, keeping its last list: {}", shop, e);
                            failed = true;
                        }
                    }
                }
                *files.write().unwrap() = last_good.concat();
            }
            Err(e) => {
                warn!("Couldn't read shops from {}: {}", path.display(), e);
                failed = true;
            }
        }
        tokio::time::sleep(if failed { RETRY_INTERVAL } else { REFRESH_INTERVAL }).await;
    }
}

/// Shop URLs from a Tinfoil locations.conf-style file: a JSON list of
/// {"url": ..., "enabled": 1} objects or plain URL strings. Local paths
/// (sdmc:/, ...), disabled entries and shops that fail to load are skipped.
fn read_shops(path: &Path) -> Result<Vec<Url>, Error> {
    let text = std::fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&text)?;
    let entries = value.get("locations").unwrap_or(&value).as_array()
        .ok_or("expected a JSON list of shops")?;

    let mut shops = Vec::new();
    for entry in entries {
        let (url, enabled) = match entry {
            Value::String(url) => (url.as_str(), true),
            Value::Object(shop) => (
                shop.get("url").and_then(Value::as_str).unwrap_or_default(),
                shop.get("enabled").map_or(true, |e| e.as_i64() != Some(0) && e.as_bool() != Some(false)),
            ),
            _ => continue,
        };
        let Ok(url) = Url::parse(url) else { continue };
        if !enabled || !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        shops.push(url);
    }
    Ok(shops)
}

/// All files in a shop, following its "directories" to other index files.
async fn fetch_shop(client: &reqwest::Client, shop: &Url) -> Result<Vec<ShopFile>, Error> {
    let mut files = Vec::new();
    let mut pending = vec![(shop.clone(), 0)];
    let mut seen = std::collections::HashSet::new();

    while let Some((index_url, depth)) = pending.pop() {
        if !seen.insert(index_url.clone()) {
            continue;
        }
        let body = client.get(index_url.clone()).send().await?.error_for_status()?.bytes().await?;
        if body.starts_with(b"TINFOIL") {
            return Err("encrypted Tinfoil index, not supported".into());
        }
        let index: Value = serde_json::from_slice(&body)?;
        let (index_files, directories) = parse_index(&index_url, &index);
        files.extend(index_files);

        if depth < MAX_DEPTH {
            for directory in directories {
                // Follow all directory links (local and public shops)
                pending.push((directory, depth + 1));
            }
        }
    }
    Ok(files)
}

/// Files and directory links in one index, with URLs made absolute.
fn parse_index(index_url: &Url, index: &Value) -> (Vec<ShopFile>, Vec<Url>) {
    let mut files = Vec::new();
    for entry in index.get("files").and_then(Value::as_array).into_iter().flatten() {
        let (url, size, name) = match entry {
            Value::String(url) => (url.as_str(), 0, None),
            Value::Object(file) => (
                file.get("url").and_then(Value::as_str).unwrap_or_default(),
                file.get("size").and_then(Value::as_u64).unwrap_or(0),
                file.get("name").and_then(Value::as_str),
            ),
            _ => continue,
        };
        let Ok(mut url) = index_url.join(url) else { continue };

        // Tinfoil names files after the URL fragment, then the last path segment
        let name = url.fragment().map(percent_decode)
            .or_else(|| name.map(String::from))
            .or_else(|| url.path_segments()?.last().map(percent_decode))
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        if url.fragment().is_none() {
            url.set_fragment(Some(&name));
        }
        files.push(ShopFile { url: url.to_string(), name, size });
    }

    let directories = index.get("directories").and_then(Value::as_array).into_iter().flatten()
        .filter_map(|d| d.as_str().or_else(|| d.get("url")?.as_str()))
        .filter_map(|d| index_url.join(d).ok())
        .collect();

    (files, directories)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_locations_conf() {
        let path = std::env::temp_dir().join(format!("aiofoil-shops-{}.conf", std::process::id()));
        std::fs::write(&path, r#"["sdmc:/", "usb:/",
            {"url": "http://192.168.1.221:8465/", "title": "homelab", "enabled": 1},
            {"url": "http://10.0.0.5/shop", "title": "off", "enabled": 0},
            {"url": "https://shop.example.com/", "title": "public", "enabled": 1},
            "http://nas.local:8080/"]"#).unwrap();
        let shops: Vec<String> = read_shops(&path).unwrap().iter().map(Url::to_string).collect();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(shops, ["http://192.168.1.221:8465/", "https://shop.example.com/", "http://nas.local:8080/"]);
    }

    #[test]
    fn parses_index_files() {
        let index_url = Url::parse("http://192.168.1.5:8000/api/shop").unwrap();
        let index = serde_json::json!({
            "files": [
                {"url": "/files/Hades%20[01000D200AC0C000][v0].nsp", "size": 10},
                {"url": "http://192.168.1.5:8000/dl/1#Celeste [01002B30028F6000][v0].nsp", "size": 20},
                "games/Tetris%2099.nsp",
            ],
            "directories": ["more.json"],
        });
        let (files, directories) = parse_index(&index_url, &index);
        assert_eq!(files[0].name, "Hades [01000D200AC0C000][v0].nsp");
        assert_eq!(files[0].url, "http://192.168.1.5:8000/files/Hades%20[01000D200AC0C000][v0].nsp#Hades%20[01000D200AC0C000][v0].nsp");
        assert_eq!(files[1].name, "Celeste [01002B30028F6000][v0].nsp");
        assert_eq!(files[1].size, 20);
        assert_eq!(files[2].name, "Tetris 99.nsp");
        assert_eq!(files[2].url, "http://192.168.1.5:8000/api/games/Tetris%2099.nsp#Tetris%2099.nsp");
        assert_eq!(directories, [Url::parse("http://192.168.1.5:8000/api/more.json").unwrap()]);
    }
}
