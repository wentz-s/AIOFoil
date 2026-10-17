# AIOFoil

A self-hosted [Tinfoil](https://tinfoil.io/) shop for the Nintendo Switch. AIOFoil collects Switch
releases from public torrent indexes, tags them with title IDs, and downloads them through your
[TorBox](https://torbox.app/) account, so Tinfoil can install them over plain HTTP.

## How it works

1. **Catalog.** Every few hours AIOFoil searches its sources for Switch torrents, drops look-alikes
   (TV episodes, PC emulators, 7z archives), and stores the rest in a local SQLite cache.
2. **Title IDs.** IDs come from the real file names inside each torrent (as reported by TorBox),
   from IDs in the torrent name, or by matching the name against
   [blawar/titledb](https://github.com/blawar/titledb).
3. **One entry per file.** Once TorBox knows a torrent's contents, the base game, each update and
   each DLC become separate entries in Tinfoil.
4. **Downloads.** When you pick a file, AIOFoil adds the magnet to TorBox and streams the file from
   TorBox's CDN to the Switch, passing Tinfoil's range requests through.
5. **ZIPs and RARs.** Torrents that hold archives instead of loose files work too. NSP/NSZ/XCI
   files are encrypted, so they're almost always archived without compression. The first time
   you pick such a torrent, AIOFoil reads the archive headers with a few small range requests
   and lists every stored file inside as its own entry. Files are streamed straight out of the
   archive, including RAR sets split into volumes (`.part1.rar`, `.part2.rar`, ... or `.rar`,
   `.r00`, ...). RAR4 and RAR5 are supported; compressed, solid or password-protected archives,
   and 7z, are not.

## Sources

| Source | Notes |
|---|---|
| `knaben` | Knaben meta-search: RuTracker, The Pirate Bay, Nyaa and more |
| `apibay` | The Pirate Bay's own API |
| `torrents-csv` | [torrents-csv.com](https://torrents-csv.com), torrents seen on the DHT |
| `nyaa` | Nyaa's "Software - Games" category (mostly overlaps with Knaben) |
| `1337x` | 1337x's Switch category, through [FlareSolverr](https://github.com/FlareSolverr/FlareSolverr) |
| `tgx` | TorrentGalaxy's Games category (torrentgalaxy.one) |

All sources are on by default. If one fails (for example FlareSolverr isn't running), AIOFoil logs a
warning and carries on with the others.

## Requirements

- [Rust](https://rustup.rs/) 1.80 or newer
- A [TorBox](https://torbox.app/) account and API key (Settings → API). Without one the game list
  still loads, but downloads won't work.
- Optional, for the `1337x` source: Docker, to run FlareSolverr

## Quick start

```sh
git clone https://github.com/wentz-s/AIOFoil.git
cd AIOFoil
cargo build --release

export TORBOX_API_KEY=your_key_here     # fish: set -x TORBOX_API_KEY your_key_here
./target/release/aiofoil
```

The server starts straight away and fills its game list in the background. The first run downloads
titledb (~90 MB) and, with the `1337x` source, opens each 1337x torrent page once to get its info
hash. That takes about 15 minutes; later refreshes only open pages for new torrents.

### FlareSolverr (for 1337x)

```sh
docker run -d --name flaresolverr --restart unless-stopped \
  -p 127.0.0.1:8191:8191 ghcr.io/flaresolverr/flaresolverr:latest
```

## Connecting Tinfoil

In Tinfoil, open **File Browser**, press **-** to add a location, and enter:

| Field | Value |
|---|---|
| Protocol | `http` |
| Host | your PC's LAN IP, e.g. `192.168.1.20` (no `http://`, no port) |
| Port | `8465` |
| Path | `/` |
| Username / Password | leave empty |

If Tinfoil can't connect, check that your firewall allows the port, for example on ufw:

```sh
sudo ufw allow from 192.168.1.0/24 to any port 8465 proto tcp
```

Every request is logged, so the server log shows whether the Switch is reaching it.

## Options

Every option can also be set with the environment variable shown.

| Option | Env | Default | |
|---|---|---|---|
| `--host` | `TINFOIL_HOST` | `0.0.0.0` | Address to listen on |
| `--port` | `TINFOIL_PORT` | `8465` | Port to serve on |
| `--public-url` | `TINFOIL_PUBLIC_URL` | request's Host header | Base URL the Switch uses to reach the server |
| `--torbox-api-key` | `TORBOX_API_KEY` | | Required for downloads |
| `--sources` | | all | Comma-separated list from the table above |
| `--min-seeders` | | `1` | Skip torrents with fewer seeders |
| `--refresh-hours` | | `6` | How often to refresh the game list |
| `--flaresolverr-url` | `FLARESOLVERR_URL` | `http://localhost:8191/v1` | FlareSolverr API |
| `--cache-path` | | `~/.cache/aiofoil/games.db` | Where the cache lives |

Run `aiofoil --help` for the full list. Set `RUST_LOG=debug` for more detailed logs.

## Endpoints

| Path | |
|---|---|
| `/` or `/index.json` | Tinfoil shop index |
| `/dl/<hash>` | Download a torrent whose contents aren't known yet (largest Switch file) |
| `/dl/<hash>/<n>` | Download the n-th Switch file in a torrent |

## Things to know

- **Uncached torrents.** If TorBox doesn't have a torrent cached yet, Tinfoil gets a "still
  downloading" error. Try again once TorBox has fetched it.
- **Firmware.** Newer games need newer Switch firmware. A game that installs fine but shows a black
  screen on launch usually needs a firmware update.
- **Coverage.** Public torrent indexes cover the popular part of the Switch library, roughly 1,400
  distinct games, not the full eShop catalog.
- **Downloads go through your PC.** Tinfoil doesn't follow redirects to other hosts, so files are
  streamed through AIOFoil instead of linking straight to TorBox.

## Development

```sh
cargo test
```

The code lives in `src/`: one module per source (`knaben.rs`, `apibay.rs`, `torrents_csv.rs`,
`nyaa.rs`, `leetx.rs`), `aggregator.rs` merges them, `torbox.rs` talks to TorBox, `titledb.rs`
matches title IDs, `cache.rs` is the SQLite store and `tinfoil.rs` is the HTTP server.

## License

[WTFPL](LICENSE): do what you want with it.

## Disclaimer

AIOFoil is a search and download tool. You are responsible for what you download with it and for
following the laws where you live.
