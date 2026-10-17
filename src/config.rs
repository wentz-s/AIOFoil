// Config module - everything comes from CLI flags or environment variables
use crate::sources::Source;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(name = "aiofoil")]
#[command(about = "AIOFoil: a Tinfoil shop for Switch games from public torrent indexes, downloaded through TorBox")]
pub struct Config {
    /// Address to listen on
    #[arg(long, env = "TINFOIL_HOST", default_value = "0.0.0.0")]
    pub host: String,

    /// Port to serve on
    #[arg(long, env = "TINFOIL_PORT", default_value_t = 8465)]
    pub port: u16,

    /// Public URL for Tinfoil (how the Switch reaches this server).
    /// Defaults to the Host header of each request.
    #[arg(long, env = "TINFOIL_PUBLIC_URL")]
    pub public_url: Option<String>,

    /// TorBox API key - required for downloads (torbox.app/settings)
    #[arg(long, env = "TORBOX_API_KEY", hide_env_values = true)]
    pub torbox_api_key: Option<String>,

    /// Where to look for Switch torrents (comma-separated)
    #[arg(long, value_enum, value_delimiter = ',', default_value = "knaben,apibay,torrents-csv,nyaa,1337x")]
    pub sources: Vec<Source>,

    /// Knaben API URL
    #[arg(long, default_value = "https://api.knaben.org/v1")]
    pub knaben_url: String,

    /// Knaben category to index (7009000 = Console / Switch)
    #[arg(long, default_value_t = 7009000)]
    pub knaben_category: u64,

    /// Skip torrents with fewer seeders than this
    #[arg(long, default_value_t = 1)]
    pub min_seeders: u64,

    /// How often to refresh the game list, in hours
    #[arg(long, default_value_t = 6)]
    pub refresh_hours: u64,

    /// FlareSolverr API, used to get past Cloudflare on 1337x
    #[arg(long, env = "FLARESOLVERR_URL", default_value = "http://localhost:8191/v1")]
    pub flaresolverr_url: String,

    /// Cache database path
    #[arg(long, default_value = DEFAULT_CACHE_PATH)]
    pub cache_path: String,
}

const DEFAULT_CACHE_PATH: &str = "~/.cache/aiofoil/games.db";
// Where the cache lived before the rename to AIOFoil
const OLD_CACHE_DIR: &str = "~/.cache/tinfoil-aggregator";

impl Config {
    /// Cache database path with `~` expanded
    pub fn cache_file(&self) -> PathBuf {
        expand_home(&self.cache_path)
    }

    /// Move the cache from its pre-rename location, so the saved 1337x hashes and
    /// TorBox file lists carry over. Only applies to the default path.
    pub fn migrate_old_cache(&self) -> std::io::Result<bool> {
        let (old, new) = (expand_home(OLD_CACHE_DIR), self.cache_dir());
        if self.cache_path != DEFAULT_CACHE_PATH || new.exists() || !old.is_dir() {
            return Ok(false);
        }
        if let Some(parent) = new.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(old, new)?;
        Ok(true)
    }

    /// Directory for the database and downloaded titledb
    pub fn cache_dir(&self) -> PathBuf {
        self.cache_file().parent().map(PathBuf::from).unwrap_or_default()
    }
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) => PathBuf::from(format!("{}{}", std::env::var("HOME").unwrap_or_default(), rest)),
        None => PathBuf::from(path),
    }
}
