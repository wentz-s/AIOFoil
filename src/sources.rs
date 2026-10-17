// Torrent sources - what every search backend returns, and the filter that
// decides whether a result is really a Switch game
use clap::ValueEnum;
use reqwest::Client;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Title searches that catch Switch releases
pub const SEARCH_QUERIES: [&str; 5] = ["nsp", "nsz", "xci", "nintendo switch", "switch"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Source {
    /// Knaben meta-search (RuTracker, The Pirate Bay, Nyaa, ...)
    Knaben,
    /// The Pirate Bay's own API
    Apibay,
    /// torrents-csv.com, torrents found on the DHT
    TorrentsCsv,
    /// Nyaa's "Software - Games" category
    Nyaa,
    /// 1337x, through FlareSolverr (--flaresolverr-url)
    #[value(name = "1337x")]
    Leetx,
    /// TorrentGalaxy's Games category
    Tgx,
}

/// One search result, whatever source it came from
#[derive(Debug, Clone)]
pub struct Hit {
    pub title: String,
    pub hash: String,
    pub size: u64,
    pub seeders: u64,
    pub magnet: Option<String>,
}

pub fn http_client() -> Client {
    Client::builder()
        .user_agent(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("failed to build HTTP client")
}

/// Decide whether a search hit is really a Switch game. Searching for "switch"
/// or "nsp" also finds TV episodes, films and albums. `category_ids` are
/// Knaben categories; pass an empty slice for sources without them.
pub fn looks_like_switch(title: &str, category_ids: &[u64]) -> bool {
    // Top-level Knaben categories that can hold games: 4 = PC, 7 = Console, 9 = Other
    const GAME_CATEGORIES: [u64; 3] = [4, 7, 9];
    if !category_ids.is_empty()
        && !category_ids.iter().any(|c| GAME_CATEGORIES.contains(&(c / 1_000_000)))
    {
        return false;
    }

    let lower = title.to_lowercase();
    if lower.contains("emulator") {
        return false;
    }

    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();

    // "NSP" is also a video release group: "The.Punisher.S02.1080p.x265-NSP"
    let is_video = words.iter().any(|w| {
        matches!(*w, "720p" | "1080p" | "2160p" | "x264" | "x265" | "h264" | "h265" | "hevc"
            | "webrip" | "webdl" | "bluray" | "brrip" | "hdtv" | "mkv" | "mp4" | "flac" | "mp3")
            || is_episode_tag(w)
    });
    if is_video {
        return false;
    }

    let has_format_tag = words.iter()
        .any(|w| matches!(*w, "nsp" | "nsz" | "xci" | "xcz" | "nsw"));

    has_format_tag
        || lower.contains("[switch")
        || lower.contains("(switch")
        || lower.contains("for nintendo switch")
}

/// Titles like "Tears of the Kingdom [XCI].7z": the torrent holds a 7z archive,
/// which can't be streamed to Tinfoil (it's solid-compressed). ZIPs and RARs can,
/// when their files are stored uncompressed, which is the norm.
pub fn is_archive_title(title: &str) -> bool {
    title.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word == "7z")
}

/// "s02", "s01e04"
fn is_episode_tag(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next() == Some('s')
        && word.len() >= 3
        && chars.all(|c| c.is_ascii_digit() || c == 'e')
        && word[1..].starts_with(|c: char| c.is_ascii_digit())
}

/// Magnet for sources that only give a hash, with a few public trackers so
/// TorBox doesn't have to rely on the DHT alone.
pub fn magnet_for(hash: &str, title: &str) -> String {
    const TRACKERS: [&str; 4] = [
        "udp://tracker.opentrackr.org:1337/announce",
        "udp://open.stealth.si:80/announce",
        "udp://tracker.torrent.eu.org:451/announce",
        "udp://exodus.desync.com:6969/announce",
    ];
    let mut magnet = format!("magnet:?xt=urn:btih:{hash}&dn={}", url_encode(title));
    for tracker in TRACKERS {
        magnet.push_str("&tr=");
        magnet.push_str(&url_encode(tracker));
    }
    magnet
}

/// "12.8 GB", "386.6 MiB" -> bytes. Torrent sites use binary units either way.
pub fn parse_size(text: &str) -> u64 {
    let mut parts = text.split_whitespace();
    let (Some(number), Some(unit)) = (parts.next(), parts.next()) else { return 0 };
    let Ok(number) = number.replace(',', "").parse::<f64>() else { return 0 };
    let multiplier: f64 = match unit.trim_end_matches("iB").trim_end_matches('B').to_uppercase().as_str() {
        "K" => 1024.0,
        "M" => 1024.0 * 1024.0,
        "G" => 1024.0 * 1024.0 * 1024.0,
        "T" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    (number * multiplier) as u64
}

/// Decode the HTML/XML entities that show up in torrent titles, including
/// numeric ones like "&#39;" and "&#x27;".
pub fn html_unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let decoded = rest[1..].find(';').filter(|&end| end <= 10).and_then(|end| {
            let entity = &rest[1..=end];
            let c = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => entity.strip_prefix('#').and_then(|num| match num.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => num.parse().ok(),
                }).and_then(char::from_u32),
            }?;
            Some((c, end + 2))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_switch_releases() {
        assert!(looks_like_switch("[Switch NSP] Mario Kart 8 Deluxe", &[4001000]));
        assert!(looks_like_switch("Super Mario Party [010036B0034E4000][v0].nsp", &[4001000]));
        assert!(looks_like_switch("PUYO PUYO TETRIS S FOR NINTENDO SWITCH", &[4001000]));
        assert!(looks_like_switch("01006a800016e000.nsp", &[7000000]));
        assert!(looks_like_switch("Hogwarts Legacy [NSP]", &[]));
        assert!(looks_like_switch("Super Mario Odyssey + Update [NSP]", &[]));
    }

    #[test]
    fn drops_lookalikes() {
        assert!(!looks_like_switch("Family.Switch.2023.1080p.WEB.h264-EDITH[TGx]", &[3001000]));
        assert!(!looks_like_switch("Kill Switch (PC) *Direct Play*", &[4001000]));
        assert!(!looks_like_switch("Life is Strange, v1.0.3 + Sudachi/Torzu Switch Emulators", &[4001000]));
        assert!(!looks_like_switch("Marvel's.The.Punisher.S02.MULTi.1080p.10bit.WEBRip.x265-NSP", &[]));
        assert!(!looks_like_switch("Some.Show.S01E04.NSP", &[]));
    }

    #[test]
    fn spots_archives() {
        assert!(is_archive_title("Tears of the Kingdom [XCI].7z"));
        assert!(!is_archive_title("Metroid Dread (NSP)(Base Game).rar"));
        assert!(!is_archive_title("Super Mario Party [NSP + Update 1.0.1]"));
        assert!(!is_archive_title("[RARBG] something [NSP]"));
        assert!(!is_archive_title("Super Smash Bros Ultimate [NSZ].zip"));
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("12.8 GB"), (12.8 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(parse_size("386.6 MiB"), (386.6 * 1024.0 * 1024.0) as u64);
        assert_eq!(parse_size("1,024 KB"), 1024 * 1024);
        assert_eq!(parse_size("garbage"), 0);
    }

    #[test]
    fn unescapes_titles() {
        assert_eq!(html_unescape("Mario &amp; Luigi &#34;RPG&#34;"), "Mario & Luigi \"RPG\"");
        assert_eq!(html_unescape("&amp;lt;"), "&lt;");
        assert_eq!(html_unescape("Luigi&#x27;s Mansion 3 &#039;22"), "Luigi's Mansion 3 '22");
        assert_eq!(html_unescape("Tom & Jerry &unknown; &"), "Tom & Jerry &unknown; &");
    }

    #[test]
    fn builds_magnets() {
        let m = magnet_for("ABC123", "Hades [NSP]");
        assert!(m.starts_with("magnet:?xt=urn:btih:ABC123&dn=Hades%20%5BNSP%5D&tr=udp%3A%2F%2F"));
    }
}
