// Title database - maps game names to Switch title IDs using blawar/titledb
use crate::sources::html_unescape;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tracing::{info, warn};

const TITLEDB_URL: &str = "https://raw.githubusercontent.com/blawar/titledb/master/US.en.json";
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Loaded once by the aggregator, read by the server for release dates
pub type SharedTitleDb = Arc<OnceLock<TitleDb>>;

#[derive(Deserialize)]
struct Entry {
    id: Option<String>,
    name: Option<String>,
    #[serde(rename = "releaseDate")]
    release_date: Option<u32>,
}

pub struct TitleDb {
    /// normalized name -> base game title ID
    by_name: HashMap<String, String>,
    /// normalized name without trailing edition words ("deluxe", "complete edition", ...)
    /// -> title ID. None where two different games share the short name.
    by_short_name: HashMap<String, Option<String>>,
    /// title ID (any type) -> release date as YYYYMMDD
    release_dates: HashMap<String, u32>,
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
        let entries: Vec<Entry> = serde_json::from_reader::<_, HashMap<String, Entry>>(file)?
            .into_values()
            .collect();
        let release_dates = entries.iter()
            .filter_map(|e| Some((e.id.as_ref()?.to_uppercase(), e.release_date.filter(|&d| d > 0)?)))
            .collect();
        let mut db = Self::from_names(entries.into_iter().filter_map(|e| Some((e.id?, e.name?))));
        db.release_dates = release_dates;
        Ok(db)
    }

    /// Release date (YYYYMMDD) of a title, falling back to its base game's for
    /// updates and DLC that titledb has no date for.
    pub fn release_date(&self, title_id: &str) -> Option<u32> {
        let id = title_id.to_uppercase();
        self.release_dates.get(&id)
            .or_else(|| self.release_dates.get(&base_title_id(&id)?))
            .copied()
    }

    fn from_names(names: impl Iterator<Item = (String, String)>) -> Self {
        // Sorted, so the same name always maps to the same ID
        let mut names: Vec<(String, String)> = names
            // Base games end in 000; updates end in 800 and DLC have their own IDs
            .filter(|(id, _)| id.len() == 16 && id.ends_with("000"))
            .map(|(id, name)| (id.to_uppercase(), normalize(&name)))
            .filter(|(_, name)| !name.is_empty())
            .collect();
        names.sort();

        let mut by_name = HashMap::new();
        let mut by_short_name: HashMap<String, Option<String>> = HashMap::new();
        for (id, name) in names {
            let short = strip_editions(&name);
            match by_short_name.get(&short) {
                Some(Some(existing)) if *existing != id => {
                    by_short_name.insert(short, None);
                }
                Some(_) => {}
                None => {
                    by_short_name.insert(short, Some(id.clone()));
                }
            }
            by_name.entry(name).or_insert(id);
        }

        info!("Loaded {} titles from titledb", by_name.len());
        Self { by_name, by_short_name, release_dates: HashMap::new() }
    }

    /// Find the base game title ID for a torrent title like
    /// "[Nintendo Switch] The Legend of Zelda: Tears of the Kingdom [NSP][RUS/Multi10]".
    /// Tries exact names first, then names without edition words, for each
    /// candidate name in the title. Update-only and DLC-only releases get none.
    pub fn lookup(&self, torrent_title: &str) -> Option<&str> {
        if is_update_or_dlc_only(torrent_title) {
            return None;
        }
        let candidates: Vec<String> = candidates(torrent_title).iter().map(|c| normalize(c)).collect();

        candidates.iter()
            .find_map(|c| self.by_name.get(c))
            .or_else(|| candidates.iter().find_map(|c| self.by_short_name.get(&strip_editions(c))?.as_ref()))
            .map(String::as_str)
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

/// Words that can trail a game's name without changing which game it is.
/// Numbers are never in here, so "Game 2" never matches "Game".
const EDITION_WORDS: [&str; 22] = [
    "edition", "complete", "definitive", "deluxe", "remastered", "remaster", "hd", "goty",
    "ultimate", "premium", "gold", "special", "standard", "digital", "anniversary",
    "nintendo", "switch", "eshop", "base", "version", "game", "year",
];

/// Release tags that aren't part of any game's name
const RELEASE_WORDS: [&str; 12] = ["nsp", "nsz", "xci", "xcz", "nsw", "repack", "usa", "eur", "jpn", "us", "eu", "multi"];

/// Names worth trying for a torrent title, most likely first.
fn candidates(title: &str) -> Vec<String> {
    let clean = strip_versions(&strip_tags(&scene_to_words(&html_unescape(title))));
    let is_list = clean.contains(" / ") || clean.contains(" | ");

    // "Ведьмак 3 / The Witcher 3": later names are only tried when the first one is
    // a translation. Otherwise they're other games in a collection, and naming the
    // torrent after one of them would be wrong.
    let listed = clean.replace(" | ", " / ");
    let segments: Vec<&str> = listed.split(" / ").collect();
    let first_is_translation = !segments[0].chars().any(|c| c.is_ascii_alphabetic());
    let segments = if first_is_translation { segments } else { segments[..1].to_vec() };

    let mut out = vec![clean.clone()];
    for segment in segments {
        let mut names = vec![segment.to_string()];
        // "Game + DLC" (after the full name, which may itself contain "+")
        if let Some((before, _)) = segment.split_once(" + ") {
            names.push(before.to_string());
        }
        // "Collection: First Game / Second Game": cut back to the collection's name.
        // Only for lists, since cutting "Metal Gear Solid: Master Collection" would
        // name a different game.
        if is_list {
            for name in names.clone() {
                let mut rest = name.as_str();
                while let Some(cut) = [": ", " - "].iter().filter_map(|sep| rest.rfind(sep)).max() {
                    rest = &rest[..cut];
                    names.push(rest.to_string());
                }
            }
        }
        out.extend(names);
    }

    let mut seen = std::collections::HashSet::new();
    out.retain(|c| !c.trim().is_empty() && seen.insert(c.trim().to_string()));
    out
}

/// Drop version numbers: "Special Edition 1.3.1" -> "Special Edition", "v1.0.3", "v131072".
fn strip_versions(title: &str) -> String {
    title.split(' ')
        .filter(|word| {
            let digits = word.strip_prefix(['v', 'V']).unwrap_or(word);
            let is_number = !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.');
            let looks_like_version = word.starts_with(['v', 'V']) || digits.matches('.').count() >= 2;
            !(is_number && looks_like_version)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// "Farming_Simulator_26_Update_v1.5.0.0_NSW-TRB" -> "Farming Simulator 26 Update v1.5.0.0 NSW".
/// Scene release names use underscores or dots instead of spaces and end in "-GROUP".
fn scene_to_words(title: &str) -> String {
    let title = title.trim();
    if title.contains(' ') || !(title.contains('_') || title.matches('.').count() >= 2) {
        return title.to_string();
    }
    let without_group = match title.rsplit_once('-') {
        Some((name, group)) if !group.is_empty() && group.chars().all(|c| c.is_ascii_alphanumeric()) => name,
        _ => title,
    };
    without_group.replace(['_', '.'], " ")
}

/// Drop "[...]" and "(...)" tags.
fn strip_tags(title: &str) -> String {
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
    name.trim().to_string()
}

/// "Mortal Kombat 11 [Update 1.0.16 Only]", "[DLC] Super Smash Bros. Ultimate" or
/// "Game_Update_v1.5_NSW-TRB": nothing but an update or DLC, which has its own
/// title ID, so the base game's would be wrong. A title that just mentions an
/// update ("Game [NSP][Update 1.1.2]") usually has the base game too.
fn is_update_or_dlc_only(title: &str) -> bool {
    let lower = title.trim().to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let mentions = words.iter().any(|w| matches!(*w, "update" | "upd" | "dlc" | "dlcs"));
    if !mentions {
        return false;
    }
    let says_only = words.contains(&"only");
    let starts_with_tag = ["[update", "[upd", "[dlc"].iter().any(|tag| lower.starts_with(tag));
    let scene_update = !lower.contains(' ') && !lower.contains("base");
    says_only || starts_with_tag || scene_update
}

/// Lowercase ASCII words, so "The Pokémon™: Let's Go II" == "pokemon lets go 2".
/// "the", release tags and Roman numerals (as digits) are normalized away, since
/// uploaders add and drop "the" freely ("Revenant Kingdom The Prince's Edition").
fn normalize(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        match c {
            '™' | '®' | '©' | '\'' | '’' | '‘' => {}
            '&' => out.push_str(" and "),
            c if c.is_ascii_alphanumeric() => out.push(c.to_ascii_lowercase()),
            c => match fold_accent(c) {
                Some(plain) => out.push(plain),
                None => out.push(' '),
            },
        }
    }

    let words: Vec<&str> = out.split_whitespace()
        .filter(|w| !RELEASE_WORDS.contains(w))
        .map(roman_to_digits)
        .collect();
    // Keep "the" when it's the whole name
    if words.len() > 1 {
        words.into_iter().filter(|w| *w != "the").collect::<Vec<_>>().join(" ")
    } else {
        words.join(" ")
    }
}

/// "mortal kombat 1 premium definitive edition" -> "mortal kombat 1"
fn strip_editions(name: &str) -> String {
    let mut words: Vec<&str> = name.split(' ').collect();
    while words.len() > 1 && words.last().is_some_and(|w| EDITION_WORDS.contains(w) || *w == "of") {
        words.pop();
    }
    words.join(" ")
}

fn roman_to_digits(word: &str) -> &str {
    match word {
        "ii" => "2",
        "iii" => "3",
        "iv" => "4",
        "v" => "5",
        "vi" => "6",
        "vii" => "7",
        "viii" => "8",
        "ix" => "9",
        "x" => "10",
        other => other,
    }
}

fn fold_accent(c: char) -> Option<char> {
    Some(match c.to_lowercase().next()? {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'ç' => 'c',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ñ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ý' | 'ÿ' => 'y',
        _ => return None,
    })
}

/// Pull a title ID like "0100F2C0115B6000" out of a file name such as
/// "Zelda [0100F2C0115B6000][v0].nsp" or "Dragon Quest III (01003E601E324000).xci".
pub fn title_id_in(file_name: &str) -> Option<&str> {
    file_name.split(['[', '('])
        .filter_map(|part| part.split([']', ')']).next())
        .find(|id| id.len() == 16 && id.starts_with("01") && id.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Version number from a file name like "Hades [01000D200AC0C000][v131072].nsp".
pub fn version_in(file_name: &str) -> Option<u32> {
    file_name.split('[')
        .filter_map(|part| part.split(']').next()?.strip_prefix(['v', 'V']))
        .find_map(|version| version.parse().ok())
}

/// Base game ID for an update (base + 0x800) or DLC (base + 0x1000 + n) ID.
pub fn base_title_id(title_id: &str) -> Option<String> {
    let id = u64::from_str_radix(title_id, 16).ok()?;
    let base = match id & 0xFFF {
        0 | 0x800 => id & !0xFFF,
        _ => (id & !0xFFF).checked_sub(0x1000)?,
    };
    Some(format!("{base:016X}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> TitleDb {
        TitleDb::from_names([
            ("01002B30028F6000", "Celeste"),
            ("0100A3D008C5C000", "Pokémon™ Scarlet"),
            ("01008F6008C5E000", "Pokémon™ Violet"),
            ("0100ABF008968000", "Pokémon™ Sword"),
            ("0100E7400C09A000", "Grand Theft Auto: The Trilogy – The Definitive Edition"),
            ("01008CF01BAAC000", "Mortal Kombat™ 1"),
            ("0100FAD01174E000", "Little Nightmares Complete Edition"),
            ("0100E8C008B6C000", "Little Nightmares II"),
            ("010035001CD38000", "THE WITCHER 3: WILD HUNT – COMPLETE EDITION"),
            ("010028600EBDA000", "Super Mario 3D World + Bowser’s Fury"),
            ("01001F5010DFA000", "Pokémon™ Legends: Arceus"),
            ("0100B04011742000", "Monster Hunter Rise"),
            ("0100B04011742800", "Monster Hunter Rise Update"), // not a base game
        ].into_iter().map(|(id, name)| (id.to_string(), name.to_string())))
    }

    #[test]
    fn matches_plain_and_tagged_titles() {
        let db = db();
        assert_eq!(db.lookup("[Nintendo Switch] Celeste [NSP][RUS/Multi10]"), Some("01002B30028F6000"));
        assert_eq!(db.lookup("Pokemon Scarlet [0100A3D008C5C000][v0][US].nsp"), Some("0100A3D008C5C000"));
        assert_eq!(db.lookup("[Switch NSP] Pok&eacute;mon Legends: Arceus"), Some("01001F5010DFA000"));
    }

    #[test]
    fn matches_collections_and_translations() {
        let db = db();
        assert_eq!(
            db.lookup("[Nintendo Switch] Grand Theft Auto: The Trilogy - The Definitive Edition: GTA III (3) / Vice City (VC) [NSP]"),
            Some("0100E7400C09A000"),
        );
        assert_eq!(
            db.lookup("[Nintendo Switch] Ведьмак 3: Дикая Охота - полное издание / The Witcher 3: Wild Hunt - Complete Edition [NSZ]"),
            Some("010035001CD38000"),
        );
        assert_eq!(db.lookup("[Nintendo Switch] Pokemon Scarlet / Violet + 3 DLC [NSP][RUS]"), Some("0100A3D008C5C000"));
    }

    #[test]
    fn keeps_plus_that_belongs_to_the_name() {
        assert_eq!(db().lookup("[SWITCH] Super Mario 3D World + Bowser’s Fury [XCI]"), Some("010028600EBDA000"));
    }

    #[test]
    fn ignores_edition_words_but_not_sequel_numbers() {
        let db = db();
        assert_eq!(db.lookup("[Nintendo Switch] Mortal Kombat 1 (MK 1) Premium Definitive edition + DLC [NSP]"), Some("01008CF01BAAC000"));
        assert_eq!(db.lookup("[Nintendo Switch] Little Nightmares [NSP][RUS/Multi7]"), Some("0100FAD01174E000"));
        assert_eq!(db.lookup("Little Nightmares II [NSP]"), Some("0100E8C008B6C000"), "II = 2");
        assert_eq!(db.lookup("Little Nightmares 3 [NSP]"), None, "a sequel the database doesn't know");
    }

    #[test]
    fn skips_update_and_dlc_only_releases() {
        let db = db();
        assert_eq!(db.lookup("Pokemon Sword [NSP][Update 1.3.1 Only]"), None);
        assert_eq!(db.lookup("[UPDATE] Pokemon Sword (Update v1.3.1)"), None);
        assert_eq!(db.lookup("Monster_Hunter_Rise_Update_v13.0.0_NSW-VENOM"), None);
        assert_eq!(db.lookup("Pokemon Sword [NSP + Update 1.2.1 + 2DLCs]"), Some("0100ABF008968000"));
        assert_eq!(db.lookup("Pokemon Sword [NSP][Update 1.2.1]"), Some("0100ABF008968000"), "base game plus update");
        assert_eq!(db.lookup("Monster.Hunter.Rise.NSW-VENOM"), Some("0100B04011742000"), "scene name");
    }

    #[test]
    fn prefers_the_first_game_of_a_list() {
        let db = TitleDb::from_names([
            ("01008C9014694000", "My Friend Peppa Pig"),
            ("0100FF1018E00000", "Peppa Pig: World Adventures"),
            ("010061901E1C2000", "Bike Jump"),
            ("0100C3F000BD8000", "METAL GEAR SOLID"),
        ].into_iter().map(|(id, name)| (id.to_string(), name.to_string())));

        assert_eq!(
            db.lookup("[Nintendo Switch] My Friend Peppa Pig (Мой друг Свинка Пеппа) + DLC Pirate Adventures / Peppa Pig: World Adventures [NSZ]"),
            Some("01008C9014694000"),
        );
        assert_eq!(db.lookup("[Nintendo Switch] Аркады Boombit, 20 шт.: Super Hero Driving School / Bike Jump [NSZ]"), None, "a collection");
        assert_eq!(db.lookup("[Nintendo Switch] Metal Gear Solid: Master Collection Edition, Vol. 1 (MG / MGS) [NSP]"), None, "not plain MGS");
    }

    #[test]
    fn ignores_version_numbers() {
        let db = TitleDb::from_names([("01005EB012D8C000".to_string(), "Hyper Light Drifter - Special Edition".to_string())].into_iter());
        assert_eq!(db.lookup("Hyper Light Drifter: Special Edition 1.3.1 [Multi-Language]"), Some("01005EB012D8C000"));
        assert_eq!(strip_versions("Game v1.0.3 Kingdom Hearts 1.5 v131072"), "Game Kingdom Hearts 1.5");
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize("The Legend of Zelda™: Link's Awakening"), "legend of zelda links awakening");
        assert_eq!(normalize("Ni no Kuni II Revenant Kingdom The Prince's Edition"), "ni no kuni 2 revenant kingdom princes edition");
        assert_eq!(normalize("Pokémon™ Legends: Arceus"), "pokemon legends arceus");
        assert_eq!(normalize("Ni no Kuni II: Revenant Kingdom"), "ni no kuni 2 revenant kingdom");
        assert_eq!(strip_editions("mortal kombat 1 premium definitive edition"), "mortal kombat 1");
    }

    #[test]
    fn finds_title_id_in_file_name() {
        assert_eq!(title_id_in("Hades [01000D200AC0C000][v0].nsp"), Some("01000D200AC0C000"));
        assert_eq!(title_id_in("DQ III HD-2D v0 (01003E601E324000).xci"), Some("01003E601E324000"));
        assert_eq!(title_id_in("Hades [NSP][RUS].nsp"), None);
    }

    #[test]
    fn finds_version_in_file_name() {
        assert_eq!(version_in("Hades [01000D200AC0C000][v131072].nsp"), Some(131072));
        assert_eq!(version_in("Balatro [0100CD801CE5E800][v720896][1.1.1].nsp"), Some(720896));
        assert_eq!(version_in("Hades [01000D200AC0C000].nsp"), None);
        assert_eq!(version_in("Game [vRUS].nsp"), None);
    }

    #[test]
    fn finds_base_title_id() {
        assert_eq!(base_title_id("0100554023408000").as_deref(), Some("0100554023408000"));
        assert_eq!(base_title_id("0100554023408800").as_deref(), Some("0100554023408000"));
        assert_eq!(base_title_id("0100152000023001").as_deref(), Some("0100152000022000"));
    }

    #[test]
    fn release_date_falls_back_to_base_game() {
        let mut db = db();
        db.release_dates.insert("0100152000022000".into(), 20170428);
        db.release_dates.insert("0100152000023001".into(), 20220318);
        assert_eq!(db.release_date("0100152000022800"), Some(20170428));
        assert_eq!(db.release_date("0100152000023001"), Some(20220318));
        assert_eq!(db.release_date("0100152000023002"), Some(20170428));
        assert_eq!(db.release_date("01AAAAAAAAAAA000"), None);
    }
}
