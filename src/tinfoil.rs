// Tinfoil HTTP server
use crate::cache::{Cache, Game, StoredFile};
use crate::config::Config;
use crate::sources::Error;
use crate::shops::SharedShopFiles;
use crate::titledb::{title_id_in, version_in, SharedTitleDb};
use crate::rar::{self, is_rar_volume, volume_key};
use crate::torbox::{base_name, cdn_range, is_switch_file, is_zip_file, Resolved, TorBoxClient, TorrentFile};
use crate::zip;
use futures::{StreamExt, TryStreamExt};
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::body::Body;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

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

/// Where a file's bytes are on TorBox's CDN
#[derive(Clone)]
enum Target {
    /// A whole file, proxied as it is
    File(String),
    /// A file stored inside a ZIP or across RAR volumes: its bytes, in order
    Pieces(Vec<Piece>),
}

/// A stretch of bytes in one file on the CDN
#[derive(Clone, Debug, PartialEq)]
struct Piece {
    url: String,
    start: u64,
    len: u64,
}

#[derive(Clone)]
struct AppState {
    cache: Cache,
    torbox: Option<TorBoxClient>,
    public_url: Option<String>,
    links: Arc<Mutex<HashMap<String, (Instant, Target)>>>,
    // No overall timeout: a download can stream for as long as it takes
    cdn: reqwest::Client,
    titledb: SharedTitleDb,
    shop_files: SharedShopFiles,
}

pub async fn start_server(
    config: &Config,
    cache: &Cache,
    titledb: &SharedTitleDb,
    shop_files: &SharedShopFiles,
) -> std::io::Result<()> {
    let state = AppState {
        cache: cache.clone(),
        titledb: titledb.clone(),
        shop_files: shop_files.clone(),
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

    let mut files_by_hash: HashMap<&str, Vec<&StoredFile>> = HashMap::new();
    for file in &files {
        files_by_hash.entry(&file.hash).or_default().push(file);
    }

    // Shops like CyberFoil sort by an optional releaseDate (YYYYMMDD); Tinfoil ignores it
    let titledb = state.titledb.get();
    let entry = |url: String, size: u64, title_id: Option<&str>| {
        let mut entry = serde_json::json!({ "url": url, "size": size });
        if let Some(date) = title_id.zip(titledb).and_then(|(id, db)| db.release_date(id)) {
            entry["releaseDate"] = date.into();
        }
        entry
    };

    // Tinfoil takes the file name (and title ID) from the URL fragment
    let mut entries = Vec::new();
    // Title ID + version of every file TorBox has, so shops don't list them again
    let mut torbox_titles = HashSet::new();
    for game in &games {
        match files_by_hash.get(game.hash.as_str()) {
            // File list known: one entry per NSP/NSZ/XCI so updates and DLC show up too
            Some(torrent_files) => {
                torbox_titles.extend(torrent_files.iter().filter_map(|f| title_key(&f.name)));
                let largest = torrent_files.iter().map(|f| f.size).max();
                for (i, file) in torrent_files.iter().enumerate() {
                    let mut name = sanitize(&file.name);
                    // Name-matched ID only applies to the base game, which is the largest file
                    if title_id_in(&name).is_none() && Some(file.size) == largest {
                        if let Some(id) = &game.title_id {
                            name = with_title_id(&name, id);
                        }
                    }
                    let url = format!("{}/dl/{}/{}#{}", base_url, game.hash, i, name);
                    entries.push(entry(url, file.size, title_id_in(&name)));
                }
            }
            // File list unknown until TorBox has the torrent: one entry for the whole thing
            None => entries.push(entry(
                format!("{}/dl/{}#{}", base_url, game.hash, torrent_file_name(game)),
                game.size,
                game.title_id.as_deref(),
            )),
        }
    }
    let torrent_entries = entries.len();

    // Files from other shops: linked directly, skipping what TorBox already has
    // and repeats across shops (the first shop listed wins)
    let shop_files = state.shop_files.read().unwrap().clone();
    let mut shop_titles = HashSet::new();
    for file in &shop_files {
        let key = title_key(&file.name);
        if key.as_ref().is_some_and(|key| torbox_titles.contains(key) || !shop_titles.insert(key.clone())) {
            continue;
        }
        entries.push(entry(file.url.clone(), file.size, title_id_in(&file.name)));
    }

    let mut message = format!("AIOFoil: {} files from {} torrents", torrent_entries, games.len());
    if entries.len() > torrent_entries {
        message.push_str(&format!(", {} from other shops", entries.len() - torrent_entries));
    }
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
    let cached = state.links.lock().unwrap().get(&link_key)
        .filter(|(created, _)| created.elapsed() < LINK_TTL)
        .map(|(_, target)| target.clone());
    if let Some(target) = cached {
        return serve(&state.cdn, &target, &method, &headers).await;
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
                Some(file) => Some(file),
                None => return (StatusCode::NOT_FOUND, "Unknown file").into_response(),
            },
            Err(e) => {
                error!("Failed to read cache: {}", e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "cache error").into_response();
            }
        },
        None => None,
    };

    let (torrent_id, files) = match torbox.resolve(&game.magnet).await {
        Ok(Resolved::Ready { torrent_id, files }) => (torrent_id, files),
        Ok(Resolved::Pending { state, progress }) => {
            info!("{} not ready on TorBox yet ({}, {:.0}%)", game.title, state, progress * 100.0);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("TorBox is still downloading this ({state}, {:.0}%). Try again later.", progress * 100.0),
            ).into_response();
        }
        Err(e) => {
            error!("TorBox failed for {}: {}", game.title, e);
            return (StatusCode::BAD_GATEWAY, format!("TorBox error: {e}")).into_response();
        }
    };

    match find_target(&state, torbox, &game.hash, torrent_id, &files, wanted.as_ref()).await {
        Ok(Some(target)) => {
            info!("Serving {} {}", game.title, wanted.as_ref().map_or("", |f| f.name.as_str()));
            state.links.lock().unwrap().insert(link_key, (Instant::now(), target.clone()));
            serve(&state.cdn, &target, &method, &headers).await
        }
        Ok(None) => {
            info!("{} has nothing Tinfoil can install, hiding it", game.title);
            if let Err(e) = state.cache.mark_unusable(&game.hash).await {
                error!("Failed to hide {}: {}", game.title, e);
            }
            (StatusCode::NOT_FOUND, "This torrent has no NSP/NSZ/XCI files, loose or stored in a ZIP").into_response()
        }
        Err(e) => {
            error!("Couldn't get {} from TorBox: {}", game.title, e);
            (StatusCode::BAD_GATEWAY, format!("TorBox error: {e}")).into_response()
        }
    }
}

/// Work out where the wanted file's bytes are (the largest file if none is
/// wanted). Saves the torrent's file list on the way, so the next index shows
/// each file separately. None means the torrent has nothing Tinfoil can install.
async fn find_target(
    state: &AppState,
    torbox: &TorBoxClient,
    hash: &str,
    torrent_id: u64,
    files: &[TorrentFile],
    wanted: Option<&StoredFile>,
) -> Result<Option<Target>, Error> {
    let link_for = |name: &str| {
        let file = files.iter().find(|f| base_name(&f.name) == name);
        async move {
            let file = file.ok_or("an archive is no longer in the torrent")?;
            torbox.download_link(torrent_id, file.id).await
        }
    };

    // A file we already know is inside a ZIP...
    if let Some(StoredFile { archive: Some(archive), header_offset: Some(header_offset), size, .. }) = wanted {
        let url = link_for(archive).await?;
        let start = zip::data_offset(&state.cdn, &url, *header_offset).await?;
        return Ok(Some(Target::Pieces(vec![Piece { url, start, len: *size }])));
    }
    // ...or split across RAR volumes
    if let Some(StoredFile { parts: Some(parts), .. }) = wanted {
        let mut links: HashMap<&str, String> = HashMap::new();
        let mut pieces = Vec::new();
        for part in parts {
            if !links.contains_key(part.volume.as_str()) {
                links.insert(&part.volume, link_for(&part.volume).await?);
            }
            pieces.push(Piece { url: links[part.volume.as_str()].clone(), start: part.offset, len: part.len });
        }
        return Ok(Some(Target::Pieces(pieces)));
    }

    // Loose NSP/NSZ/XCI files
    let loose: Vec<&TorrentFile> = files.iter().filter(|f| is_switch_file(&f.name)).collect();
    if let Some(largest) = loose.iter().max_by_key(|f| f.size) {
        let stored: Vec<StoredFile> = loose.iter()
            .map(|f| StoredFile {
                hash: hash.to_string(),
                name: base_name(&f.name).to_string(),
                size: f.size,
                archive: None,
                header_offset: None,
                parts: None,
            })
            .collect();
        state.cache.set_files(hash, &stored).await?;

        let file = wanted
            .and_then(|w| loose.iter().find(|f| base_name(&f.name) == w.name))
            .unwrap_or(largest);
        let url = torbox.download_link(torrent_id, file.id).await?;
        return Ok(Some(Target::File(url)));
    }

    // Otherwise look inside the torrent's ZIPs and RARs for stored (uncompressed) files
    let mut found: Vec<StoredFile> = Vec::new();
    let mut links: HashMap<String, String> = HashMap::new();

    for zip_file in files.iter().filter(|f| is_zip_file(&f.name)) {
        let name = base_name(&zip_file.name).to_string();
        let url = torbox.download_link(torrent_id, zip_file.id).await?;
        match zip::read_entries(&state.cdn, &url, zip_file.size).await {
            Ok(entries) => found.extend(entries.into_iter().map(|entry| StoredFile {
                hash: hash.to_string(),
                name: base_name(&entry.name).to_string(),
                size: entry.size,
                archive: Some(name.clone()),
                header_offset: Some(entry.header_offset),
                parts: None,
            })),
            Err(e) => warn!("Couldn't read {}: {}", name, e),
        }
        links.insert(name, url);
    }

    let mut volumes: Vec<&TorrentFile> = files.iter().filter(|f| is_rar_volume(&f.name)).collect();
    volumes.sort_by_key(|f| volume_key(base_name(&f.name)));
    let mut pieces = Vec::new();
    for volume in volumes {
        let name = base_name(&volume.name).to_string();
        let url = torbox.download_link(torrent_id, volume.id).await?;
        match rar::read_pieces(&state.cdn, &url, volume.size).await {
            Ok(found_pieces) => pieces.push((name.clone(), found_pieces)),
            Err(e) => warn!("Couldn't read {}: {}", name, e),
        }
        links.insert(name, url);
    }
    found.extend(rar::assemble(&pieces).into_iter().map(|entry| StoredFile {
        hash: hash.to_string(),
        name: base_name(&entry.name).to_string(),
        size: entry.size,
        archive: entry.parts.first().map(|p| p.volume.clone()),
        header_offset: None,
        parts: Some(entry.parts),
    }));

    let Some(largest) = found.iter().max_by_key(|f| f.size) else {
        return Ok(None);
    };
    state.cache.set_files(hash, &found).await?;
    info!("Found {} installable files inside archives", found.len());

    let target = match (&largest.archive, largest.header_offset, &largest.parts) {
        (Some(archive), Some(header_offset), _) => {
            let url = links[archive].clone();
            let start = zip::data_offset(&state.cdn, &url, header_offset).await?;
            Target::Pieces(vec![Piece { url, start, len: largest.size }])
        }
        (_, _, Some(parts)) => Target::Pieces(
            parts.iter()
                .map(|p| Piece { url: links[&p.volume].clone(), start: p.offset, len: p.len })
                .collect(),
        ),
        _ => return Ok(None),
    };
    Ok(Some(target))
}

async fn serve(client: &reqwest::Client, target: &Target, method: &Method, headers: &HeaderMap) -> Response {
    match target {
        Target::File(url) => proxy(client, url, method, headers).await,
        Target::Pieces(pieces) => proxy_pieces(client, pieces, method, headers).await,
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

/// Serve a file stored inside archives as a file of its own: work out which
/// stretches of which archive files Tinfoil's Range covers, and stream them in order.
async fn proxy_pieces(client: &reqwest::Client, pieces: &[Piece], method: &Method, headers: &HeaderMap) -> Response {
    let size: u64 = pieces.iter().map(|p| p.len).sum();
    let requested = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let range = match requested {
        Some(value) => match parse_range(value, size) {
            Some(range) => Some(range),
            None => {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                    .body(Body::empty())
                    .unwrap_or_default();
            }
        },
        None => None,
    };
    let (first, last) = range.unwrap_or((0, size.saturating_sub(1)));

    let mut response = Response::builder()
        .header(header::CONTENT_LENGTH, if size == 0 { 0 } else { last - first + 1 })
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, "application/octet-stream");
    response = match range {
        Some(_) => response
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_RANGE, format!("bytes {first}-{last}/{size}")),
        None => response.status(StatusCode::OK),
    };

    let mut ranges = upstream_ranges(pieces, first, last).into_iter();
    let first_range = match ranges.next() {
        Some(first_range) if method != Method::HEAD && size > 0 => first_range,
        _ => return response.body(Body::empty()).unwrap_or_default(),
    };

    // Start the first request now, so a failure is a clean error instead of a cut-off body
    let head = match fetch_range(client, &first_range).await {
        Ok(head) => head,
        Err(e) => {
            error!("TorBox CDN request failed: {}", e);
            return (StatusCode::BAD_GATEWAY, format!("TorBox CDN error: {e}")).into_response();
        }
    };
    let client = client.clone();
    let rest = futures::stream::iter(ranges)
        .then(move |piece| {
            let client = client.clone();
            async move { fetch_range(&client, &piece).await.map(|r| r.bytes_stream().map_err(std::io::Error::other)) }
        })
        .try_flatten();
    let body = head.bytes_stream().map_err(std::io::Error::other).chain(rest);

    response
        .body(Body::from_stream(body))
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

async fn fetch_range(client: &reqwest::Client, piece: &Piece) -> Result<reqwest::Response, std::io::Error> {
    cdn_range(client, &piece.url, piece.start, piece.start + piece.len - 1)
        .await
        .map_err(std::io::Error::other)
}

/// The stretches of the CDN files that hold bytes `first..=last` of the file made of `pieces`.
fn upstream_ranges(pieces: &[Piece], first: u64, last: u64) -> Vec<Piece> {
    let mut ranges = Vec::new();
    let mut piece_start = 0;
    for piece in pieces {
        let piece_end = piece_start + piece.len; // exclusive
        let from = first.max(piece_start);
        let to = (last + 1).min(piece_end); // exclusive
        if from < to {
            ranges.push(Piece {
                url: piece.url.clone(),
                start: piece.start + (from - piece_start),
                len: to - from,
            });
        }
        piece_start = piece_end;
    }
    ranges
}

/// Parse a "bytes=first-last", "bytes=first-" or "bytes=-suffix" header against a
/// file of `size` bytes. Only the first range of a multi-range request is used.
fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?.split(',').next()?.trim();
    let (first, last) = spec.split_once('-')?;
    let (first, last) = if first.is_empty() {
        let suffix: u64 = last.parse().ok()?;
        (size.checked_sub(suffix.min(size))?, size.checked_sub(1)?)
    } else {
        let first: u64 = first.parse().ok()?;
        let last = match last {
            "" => size.checked_sub(1)?,
            last => last.parse::<u64>().ok()?.min(size.checked_sub(1)?),
        };
        (first, last)
    };
    (first <= last && first < size && (last - first) < size).then_some((first, last))
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
/// What makes two files the same content: title ID and version. Base games
/// without a version tag are taken as v0; other files need one to match.
fn title_key(name: &str) -> Option<(String, u32)> {
    let id = title_id_in(name)?.to_uppercase();
    let version = version_in(name).or_else(|| id.ends_with("000").then_some(0))?;
    Some((id, version))
}

fn sanitize(name: &str) -> String {
    let name: String = name.chars()
        .map(|c| if "/\\:*?\"<>|#%".contains(c) { '_' } else { c })
        .collect();
    name.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_files_by_title_and_version() {
        let key = |id: &str, v| Some((id.to_string(), v));
        assert_eq!(title_key("Hades [01000D200AC0C000][v0].nsp"), key("01000D200AC0C000", 0));
        assert_eq!(title_key("Hades [01000d200ac0c000].nsp"), key("01000D200AC0C000", 0));
        assert_eq!(title_key("Hades [01000D200AC0C800][v131072].nsp"), key("01000D200AC0C800", 131072));
        assert_eq!(title_key("Hades Update [01000D200AC0C800].nsp"), None);
        assert_eq!(title_key("Hades.nsp"), None);
    }

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_range("bytes=0-1023", 5000), Some((0, 1023)));
        assert_eq!(parse_range("bytes=4000-", 5000), Some((4000, 4999)));
        assert_eq!(parse_range("bytes=-100", 5000), Some((4900, 4999)));
        assert_eq!(parse_range("bytes=4000-9999", 5000), Some((4000, 4999)), "end is clamped");
        assert_eq!(parse_range("bytes=0-10, 20-30", 5000), Some((0, 10)), "first of several");
        assert_eq!(parse_range("bytes=5000-", 5000), None, "starts past the end");
        assert_eq!(parse_range("bytes=10-5", 5000), None);
        assert_eq!(parse_range("items=0-5", 5000), None);
        assert_eq!(parse_range("bytes=0-", 0), None, "empty file");
    }

    #[test]
    fn maps_ranges_onto_pieces() {
        let piece = |url: &str, start, len| Piece { url: url.into(), start, len };
        // A 250-byte file: 100 bytes at 1000 in part1, 100 at 50 in part2, 50 at 50 in part3
        let pieces = [piece("part1", 1000, 100), piece("part2", 50, 100), piece("part3", 50, 50)];

        assert_eq!(upstream_ranges(&pieces, 0, 249), pieces.to_vec(), "the whole file");
        assert_eq!(upstream_ranges(&pieces, 10, 19), vec![piece("part1", 1010, 10)]);
        assert_eq!(
            upstream_ranges(&pieces, 95, 104),
            vec![piece("part1", 1095, 5), piece("part2", 50, 5)],
            "across a volume boundary"
        );
        assert_eq!(
            upstream_ranges(&pieces, 99, 200),
            vec![piece("part1", 1099, 1), piece("part2", 50, 100), piece("part3", 50, 1)],
        );
        assert_eq!(upstream_ranges(&pieces, 249, 249), vec![piece("part3", 99, 1)], "last byte");
    }
}
