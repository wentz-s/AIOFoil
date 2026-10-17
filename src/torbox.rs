// TorBox API client - file lists for cached torrents, and magnet -> direct HTTP download links
use reqwest::header::RANGE;
use reqwest::multipart::Form;
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;

const API_URL: &str = "https://api.torbox.app/v1/api";
const SWITCH_EXTENSIONS: [&str; 4] = [".nsp", ".nsz", ".xci", ".xcz"];
const CDN_ATTEMPTS: u32 = 5;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    success: bool,
    detail: Option<String>,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct Created {
    torrent_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Torrent {
    #[serde(default)]
    download_present: bool,
    #[serde(default)]
    download_finished: bool,
    #[serde(default)]
    download_state: String,
    #[serde(default)]
    progress: f64,
    #[serde(default)]
    files: Vec<TorrentFile>,
}

/// A file in a torrent on TorBox
#[derive(Debug, Deserialize, Clone)]
pub struct TorrentFile {
    pub id: u64,
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Deserialize)]
pub struct CachedTorrent {
    pub hash: String,
    #[serde(default)]
    pub files: Vec<FileInfo>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
}

pub enum Resolved {
    /// TorBox has the torrent; these are all its files
    Ready { torrent_id: u64, files: Vec<TorrentFile> },
    /// TorBox is still fetching the torrent
    Pending { state: String, progress: f64 },
}

#[derive(Clone)]
pub struct TorBoxClient {
    client: Client,
    api_key: String,
}

impl TorBoxClient {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
        }
    }

    /// File lists for whichever of `hashes` TorBox already has cached.
    pub async fn check_cached(&self, hashes: &[String]) -> Result<Vec<CachedTorrent>, Error> {
        let response: ApiResponse<Vec<CachedTorrent>> = self.client
            .post(format!("{API_URL}/torrents/checkcached"))
            .query(&[("format", "list"), ("list_files", "true")])
            .json(&serde_json::json!({ "hashes": hashes }))
            .bearer_auth(&self.api_key)
            .send()
            .await?
            .json()
            .await?;

        if !response.success {
            return Err(response.detail.unwrap_or_else(|| "TorBox request failed".to_string()).into());
        }
        // No data just means none of them are cached
        Ok(response.data.unwrap_or_default())
    }

    /// Add the magnet to TorBox (a no-op if it's already there) and list its files
    /// once TorBox has it.
    pub async fn resolve(&self, magnet: &str) -> Result<Resolved, Error> {
        let created: Created = self.call(
            self.client.post(format!("{API_URL}/torrents/createtorrent"))
                .multipart(Form::new().text("magnet", magnet.to_string())),
        ).await?;

        let Some(torrent_id) = created.torrent_id else {
            // Active download slots are full, TorBox queued it
            return Ok(Resolved::Pending { state: "queued".to_string(), progress: 0.0 });
        };

        let torrent: Torrent = self.call(
            self.client.get(format!("{API_URL}/torrents/mylist"))
                .query(&[("id", torrent_id.to_string()), ("bypass_cache", "true".to_string())]),
        ).await?;

        if !(torrent.download_present || torrent.download_finished) {
            return Ok(Resolved::Pending { state: torrent.download_state, progress: torrent.progress });
        }

        Ok(Resolved::Ready { torrent_id, files: torrent.files })
    }

    /// Direct download link for one file of a torrent in the account.
    pub async fn download_link(&self, torrent_id: u64, file_id: u64) -> Result<String, Error> {
        self.call(
            self.client.get(format!("{API_URL}/torrents/requestdl"))
                .query(&[
                    ("token", self.api_key.clone()),
                    ("torrent_id", torrent_id.to_string()),
                    ("file_id", file_id.to_string()),
                ]),
        ).await
    }

    async fn call<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T, Error> {
        let response: ApiResponse<T> = request
            .bearer_auth(&self.api_key)
            .send()
            .await?
            .json()
            .await?;

        if !response.success {
            return Err(response.detail.unwrap_or_else(|| "TorBox request failed".to_string()).into());
        }
        response.data.ok_or_else(|| "TorBox returned no data".into())
    }
}

/// GET bytes `start..=end` of a file on TorBox's CDN. Links that were just issued
/// sometimes answer 400 or 429 for a moment when many are requested in a row,
/// so temporary errors are retried with a growing delay.
pub async fn cdn_range(client: &Client, url: &str, start: u64, end: u64) -> Result<Response, Error> {
    let mut delay = std::time::Duration::from_millis(500);
    for attempt in 1..=CDN_ATTEMPTS {
        let last_attempt = attempt == CDN_ATTEMPTS;
        match client.get(url).header(RANGE, format!("bytes={start}-{end}")).send().await {
            Ok(response) if response.status() == StatusCode::PARTIAL_CONTENT => return Ok(response),
            Ok(response) if !last_attempt && is_temporary(response.status()) => {}
            Ok(response) => return Err(format!("expected a partial response, got HTTP {}", response.status()).into()),
            Err(e) if !last_attempt => tracing::debug!("TorBox CDN request failed, retrying: {}", e),
            Err(e) => return Err(e.into()),
        }
        tokio::time::sleep(delay).await;
        delay *= 2;
    }
    unreachable!("the last attempt always returns")
}

fn is_temporary(status: StatusCode) -> bool {
    matches!(status, StatusCode::BAD_REQUEST | StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS)
        || status.is_server_error()
}

pub fn is_zip_file(name: &str) -> bool {
    name.to_lowercase().ends_with(".zip")
}

pub fn is_switch_file(name: &str) -> bool {
    let name = name.to_lowercase();
    SWITCH_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

/// "Torrent Folder/Game [0100...][v0].nsp" -> "Game [0100...][v0].nsp"
pub fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}
