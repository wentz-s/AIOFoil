// Title database - maps game names to Switch title IDs using blawar/titledb
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

const TITLEDB_URL: &str = "https://raw.githubusercontent.com/blawar/titledb/master/US.en.json";
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Deserialize)]
struct Entry {
    id: Option<String>,
    name: Option<String>,
}

pub struct TitleDb {
    /// normalized name -> base game title ID
    by_name: HashMap<String, String>,
}

impl TitleDb {
    /// Load titledb from `cache_dir`, downloading it if missing or older than a week.
    pub async fn load(cache_dir: &Path) -> Result<Self, Error> {
        let path = cache_dir.join("titledb.json");

        let age = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok());

        if age.map_or(true, |age| age > MAX_AGE) {
            info!("Downloading titledb (~90 MB)...");
            match download(&path).await {
                Ok(()) => info!("titledb updated"),
                // A stale copy is still useful
                Err(e) if age.is_some() => warn!("titledb download failed, using old copy: {}", e),
                Err(e) => return Err(e),
            }
        }

        tokio::task::spawn_blocking(move || Self::parse(&path)).await?
    }

    fn parse(path: &Path) -> Result<Self, Error> {
        let file = std::io::BufReader::new(std::fs::File::open(path)?);
        let entries: HashMap<String, Entry> = serde_json::from_reader(file)?;

        let mut by_name = HashMap::new();
        for entry in entries.into_values() {
            let (Some(id), Some(name)) = (entry.id, entry.name) else { continue };
            // Base games end in 000; updates end in 800 and DLC have their own IDs
            if id.len() == 16 && id.ends_with("000") {
                by_name.entry(normalize(&name)).or_insert(id.to_uppercase());
            }
        }

        info!("Loaded {} titles from titledb", by_name.len());
        Ok(Self { by_name })
    }

    /// Find the base game title ID for a torrent title like
    /// "[Nintendo Switch] The Legend of Zelda: Tears of the Kingdom [NSP][RUS/Multi10]".
    pub fn lookup(&self, torrent_title: &str) -> Option<&str> {
        self.by_name.get(&normalize(&game_name(torrent_title))).map(String::as_str)
    }
}

async fn download(path: &Path) -> Result<(), Error> {
    let bytes = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?
        .get(TITLEDB_URL)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    // Write then rename so a failed download never leaves a truncated file
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Strip release tags and bundle extras: "Game [NSP] (notes) + DLC" -> "Game".
fn game_name(title: &str) -> String {
    let mut name = String::new();
    let mut depth = 0;
    for c in title.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = (depth - 1).max(0),
            _ if depth == 0 => name.push(c),
            _ => {}
        }
    }

    // Bundles: "Pokemon Scarlet / Violet", "Hades + DLC" -> first game only
    let end = [" + ", " / "].iter()
        .filter_map(|sep| name.find(sep))
        .min()
        .unwrap_or(name.len());
    name[..end].to_string()
}

/// Lowercase ASCII words only, so "Pokémon™: Let's Go" == "pokemon lets go".
fn normalize(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        match c {
            '™' | '®' | '©' | '\'' | '’' => {}
            '&' => out.push_str(" and "),
            'é' | 'è' | 'ê' | 'É' => out.push('e'),
            c if c.is_ascii_alphanumeric() => out.push(c.to_ascii_lowercase()),
            _ => out.push(' '),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Pull a title ID like "0100F2C0115B6000" out of a file name such as
/// "Zelda [0100F2C0115B6000][v0].nsp" or "Dragon Quest III (01003E601E324000).xci".
pub fn title_id_in(file_name: &str) -> Option<&str> {
    file_name.split(['[', '('])
        .filter_map(|part| part.split([']', ')']).next())
        .find(|id| id.len() == 16 && id.starts_with("01") && id.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_bundles() {
        assert_eq!(
            normalize(&game_name("[Nintendo Switch] Pokemon Scarlet / Violet + 3 DLC [NSP][RUS]")),
            "pokemon scarlet"
        );
        assert_eq!(normalize("The Legend of Zelda™: Link's Awakening"), "the legend of zelda links awakening");
        assert_eq!(normalize("Pokémon™ Legends: Arceus"), "pokemon legends arceus");
    }

    #[test]
    fn finds_title_id_in_file_name() {
        assert_eq!(title_id_in("Hades [01000D200AC0C000][v0].nsp"), Some("01000D200AC0C000"));
        assert_eq!(title_id_in("DQ III HD-2D v0 (01003E601E324000).xci"), Some("01003E601E324000"));
        assert_eq!(title_id_in("Hades [NSP][RUS].nsp"), None);
    }
}
