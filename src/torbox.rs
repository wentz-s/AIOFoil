// TorBox API client - file lists for cached torrents, and magnet -> direct HTTP download link
use reqwest::multipart::Form;
use reqwest::{Client, RequestBuilder};
use serde::de::DeserializeOwned;
use serde::Deserialize;

const API_URL: &str = "https://api.torbox.app/v1/api";
const SWITCH_EXTENSIONS: [&str; 4] = [".nsp", ".nsz", ".xci", ".xcz"];

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

#[derive(Debug, Deserialize)]
struct TorrentFile {
    id: u64,
    name: String,
    size: u64,
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
    /// Direct download link, plus every Switch file in the torrent
    Ready { url: String, files: Vec<FileInfo> },
    /// TorBox is still fetching the torrent
    Pending { state: String, progress: f64 },
    /// The torrent has no NSP/NSZ/XCI, usually because it's a RAR or 7z archive
    NoSwitchFiles,
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

    /// Add the magnet to TorBox (a no-op if it's already there) and return a
    /// download link for `wanted` (a file name), or the largest Switch file if
    /// `wanted` is None or not found.
    pub async fn resolve(&self, magnet: &str, wanted: Option<&str>) -> Result<Resolved, Error> {
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

        let switch_files: Vec<&TorrentFile> = torrent.files.iter()
            .filter(|f| is_switch_file(&f.name))
            .collect();

        let Some(file) = wanted
            .and_then(|wanted| switch_files.iter().find(|f| base_name(&f.name) == base_name(wanted)))
            .or_else(|| switch_files.iter().max_by_key(|f| f.size))
        else {
            return Ok(Resolved::NoSwitchFiles);
        };

        let url: String = self.call(
            self.client.get(format!("{API_URL}/torrents/requestdl"))
                .query(&[
                    ("token", self.api_key.clone()),
                    ("torrent_id", torrent_id.to_string()),
                    ("file_id", file.id.to_string()),
                ]),
        ).await?;

        let files = switch_files.iter()
            .map(|f| FileInfo { name: f.name.clone(), size: f.size })
            .collect();

        Ok(Resolved::Ready { url, files })
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

pub fn is_switch_file(name: &str) -> bool {
    let name = name.to_lowercase();
    SWITCH_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

/// "Torrent Folder/Game [0100...][v0].nsp" -> "Game [0100...][v0].nsp"
pub fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}
