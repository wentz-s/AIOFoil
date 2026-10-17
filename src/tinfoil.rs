// Tinfoil HTTP server
use crate::cache::{Cache, Game, TorrentFile};
use crate::config::Config;
use crate::titledb::title_id_in;
use crate::torbox::{base_name, Resolved, TorBoxClient};
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::body::Body;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tracing::{error, info};

// TorBox download links stay valid for a few hours; reuse them for a while
// so Tinfoil's many range requests for one file don't each hit the API.
const LINK_TTL: Duration = Duration::from_secs(30 * 60);

// Response headers passed through from TorBox's CDN to Tinfoil
const PROXIED_HEADERS: [header::HeaderName; 4] = [
    header::CONTENT_LENGTH,
    header::CONTENT_RANGE,
    header::ACCEPT_RANGES,
    header::CONTENT_TYPE,
];

#[derive(Clone)]
struct AppState {
    cache: Cache,
    torbox: Option<TorBoxClient>,
    public_url: Option<String>,
    links: Arc<Mutex<HashMap<String, (Instant, String)>>>,
    // No overall timeout: a download can stream for as long as it takes
    cdn: reqwest::Client,
}

pub async fn start_server(config: &Config, cache: &Cache) -> std::io::Result<()> {
    let state = AppState {
        cache: cache.clone(),
        torbox: config.torbox_api_key.clone().map(TorBoxClient::new),
        public_url: config.public_url.as_ref().map(|u| u.trim_end_matches('/').to_string()),
        links: Arc::new(Mutex::new(HashMap::new())),
        cdn: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client"),
    };

    let router = Router::new()
        .route("/", get(index))
        .route("/index.json", get(index))
        .route("/dl/:hash", get(download_torrent))
        .route("/dl/:hash/:file", get(download_file))
        .layer(middleware::from_fn(log_request))
        .with_state(state);

    let address = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&address).await?;
    info!("Tinfoil server listening on http://{}", address);

    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await
}

/// Log every request, so it's obvious whether the Switch is reaching the server at all.
async fn log_request(ConnectInfo(client): ConnectInfo<SocketAddr>, request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let agent = request.headers().get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();

    let response = next.run(request).await;
    info!("{} {} {} -> {} ({})", client.ip(), method, path, response.status().as_u16(), agent);
    response
}

/// Tinfoil shop index: https://blawar.github.io/tinfoil/custom_index/
async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (games, files) = match tokio::try_join!(state.cache.get_all_games(), state.cache.get_all_files()) {
        Ok(result) => result,
        Err(e) => {
            error!("Failed to read cache: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "cache error").into_response();
        }
    };

    let base_url = state.public_url.clone().unwrap_or_else(|| {
        let host = headers.get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost");
        format!("http://{host}")
    });

    let mut files_by_hash: HashMap<&str, Vec<&TorrentFile>> = HashMap::new();
    for file in &files {
        files_by_hash.entry(&file.hash).or_default().push(file);
    }

    // Tinfoil takes the file name (and title ID) from the URL fragment
    let mut entries = Vec::new();
    for game in &games {
        match files_by_hash.get(game.hash.as_str()) {
            // File list known: one entry per NSP/NSZ/XCI so updates and DLC show up too
            Some(torrent_files) => {
                let largest = torrent_files.iter().map(|f| f.size).max();
                for (i, file) in torrent_files.iter().enumerate() {
                    let mut name = sanitize(&file.name);
                    // Name-matched ID only applies to the base game, which is the largest file
                    if title_id_in(&name).is_none() && Some(file.size) == largest {
                        if let Some(id) = &game.title_id {
                            name = with_title_id(&name, id);
                        }
                    }
                    entries.push(serde_json::json!({
                        "url": format!("{}/dl/{}/{}#{}", base_url, game.hash, i, name),
                        "size": file.size,
                    }));
                }
            }
            // File list unknown until TorBox has the torrent: one entry for the whole thing
            None => entries.push(serde_json::json!({
                "url": format!("{}/dl/{}#{}", base_url, game.hash, torrent_file_name(game)),
                "size": game.size,
            })),
        }
    }

    let mut message = format!("AIOFoil: {} files from {} torrents", entries.len(), games.len());
    if state.torbox.is_none() {
        message.push_str("\nNo TorBox API key set - downloads won't work.");
    }

    Json(serde_json::json!({ "files": entries, "success": message })).into_response()
}

async fn download_torrent(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    download(state, hash, None, method, headers).await
}

async fn download_file(
    State(state): State<AppState>,
    Path((hash, file)): Path<(String, usize)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    download(state, hash, Some(file), method, headers).await
}

/// Stream the file from TorBox to Tinfoil. Tinfoil doesn't follow redirects
/// to other hosts, so the bytes go through this server.
async fn download(state: AppState, hash: String, file_index: Option<usize>, method: Method, headers: HeaderMap) -> Response {
    let Some(torbox) = &state.torbox else {
        return (StatusCode::SERVICE_UNAVAILABLE, "No TorBox API key configured").into_response();
    };

    let link_key = format!("{hash}/{file_index:?}");
    let cached_url = state.links.lock().unwrap().get(&link_key)
        .filter(|(created, _)| created.elapsed() < LINK_TTL)
        .map(|(_, url)| url.clone());
    if let Some(url) = cached_url {
        return proxy(&state.cdn, &url, &method, &headers).await;
    }

    let game = match state.cache.get_game(&hash).await {
        Ok(Some(game)) => game,
        Ok(None) => return (StatusCode::NOT_FOUND, "Unknown game").into_response(),
        Err(e) => {
            error!("Failed to read cache: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "cache error").into_response();
        }
    };

    let wanted = match file_index {
        Some(i) => match state.cache.get_files(&game.hash).await {
            Ok(files) => match files.into_iter().nth(i) {
                Some(file) => Some(file.name),
                None => return (StatusCode::NOT_FOUND, "Unknown file").into_response(),
            },
            Err(e) => {
                error!("Failed to read cache: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "cache error").into_response();
            }
        },
        None => None,
    };

    match torbox.resolve(&game.magnet, wanted.as_deref()).await {
        Ok(Resolved::Ready { url, files }) => {
            info!("Serving {} {}", game.title, wanted.as_deref().unwrap_or(""));
            // Remember the file list so the next index shows each file separately
            let files: Vec<(String, u64)> = files.iter()
                .map(|f| (base_name(&f.name).to_string(), f.size))
                .collect();
            if let Err(e) = state.cache.set_files(&game.hash, &files).await {
                error!("Failed to save file list: {}", e);
            }
            state.links.lock().unwrap().insert(link_key, (Instant::now(), url.clone()));
            proxy(&state.cdn, &url, &method, &headers).await
        }
        Ok(Resolved::NoSwitchFiles) => {
            info!("{} has no NSP/NSZ/XCI files, hiding it", game.title);
            if let Err(e) = state.cache.mark_unusable(&game.hash).await {
                error!("Failed to hide {}: {}", game.title, e);
            }
            (StatusCode::NOT_FOUND, "This torrent has no NSP/NSZ/XCI files (probably a RAR or 7z archive)").into_response()
        }
        Ok(Resolved::Pending { state, progress }) => {
            info!("{} not ready on TorBox yet ({}, {:.0}%)", game.title, state, progress * 100.0);
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("TorBox is still downloading this ({state}, {:.0}%). Try again later.", progress * 100.0),
            ).into_response()
        }
        Err(e) => {
            error!("TorBox failed for {}: {}", game.title, e);
            (StatusCode::BAD_GATEWAY, format!("TorBox error: {e}")).into_response()
        }
    }
}

/// Fetch `url` from TorBox's CDN and stream it back, forwarding Tinfoil's Range
/// header so it can read the file in pieces.
async fn proxy(client: &reqwest::Client, url: &str, method: &Method, headers: &HeaderMap) -> Response {
    let mut request = if method == Method::HEAD { client.head(url) } else { client.get(url) };
    if let Some(range) = headers.get(header::RANGE) {
        request = request.header(header::RANGE, range);
    }

    let upstream = match request.send().await {
        Ok(upstream) => upstream,
        Err(e) => {
            error!("TorBox CDN request failed: {}", e);
            return (StatusCode::BAD_GATEWAY, format!("TorBox CDN error: {e}")).into_response();
        }
    };

    let mut response = Response::builder().status(upstream.status());
    for name in PROXIED_HEADERS {
        if let Some(value) = upstream.headers().get(&name) {
            response = response.header(name, value);
        }
    }

    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

/// File name for a whole torrent whose contents we don't know yet.
fn torrent_file_name(game: &Game) -> String {
    let upper = game.title.to_uppercase();
    let ext = if upper.contains("NSZ") {
        "nsz"
    } else if upper.contains("XCI") {
        "xci"
    } else {
        "nsp"
    };

    let name = format!("{}.{}", sanitize(&game.title), ext);
    match &game.title_id {
        Some(id) if title_id_in(&name).is_none() => with_title_id(&name, id),
        _ => name,
    }
}

/// "Game.nsp" -> "Game [0100...][v0].nsp", the format Tinfoil reads IDs from.
fn with_title_id(name: &str, title_id: &str) -> String {
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, "nsp"));
    format!("{} [{}][v0].{}", stem.trim(), title_id, ext)
}

/// Replace characters that would break the URL or the file name.
fn sanitize(name: &str) -> String {
    let name: String = name.chars()
        .map(|c| if "/\\:*?\"<>|#%".contains(c) { '_' } else { c })
        .collect();
    name.trim().to_string()
}
