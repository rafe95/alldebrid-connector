# alldebrid-connector

A small backend that takes torrents and magnets, hands them to
[AllDebrid](https://alldebrid.com), and downloads the finished files into a folder
chosen by category. It needs an AllDebrid premium API key.

```
you / Prowlarr / Sonarr / Radarr ──▶ connector ──▶ AllDebrid ──▶ /downloads/<category>/<name>/
```

## Run

```sh
echo "ALLDEBRID_API_KEY=your-key" > .env
docker compose up -d --build
```

There is **no authentication**. Keep ports 8080 and 8081 on your LAN.

## Web UI

`compose.yaml` also starts an optional `web` service: plain HTML, CSS and JavaScript from
`web/`, served by nginx on <http://localhost:8081>, which forwards `/api/` to the
connector. Use it to paste a magnet or drop a `.torrent`, pick a category, and follow or
remove downloads. The connector works without it; delete the service if you don't need it.

## Categories

| Category | Folder |
|---|---|
| `tv` | `/downloads/tv` |
| `movies` | `/downloads/movies` |
| `adult` | `/downloads/adult` |
| `other` | `/downloads/other` |

A missing or unknown category keeps its label but lands in the `other` folder. Add or
override categories with `AD_CATEGORY_DIRS`, e.g. `sonarr=/downloads/tv,other=/misc`.

## Add things by hand

```sh
# magnet, bare info-hash, or http(s) link to a .torrent
curl -X POST localhost:8080/api/downloads -H 'content-type: application/json' \
  -d '{"magnet": "magnet:?xt=urn:btih:...", "category": "movies"}'

# .torrent file
curl -X POST localhost:8080/api/downloads/torrent -F category=tv -F torrent=@file.torrent

curl localhost:8080/api/downloads                             # list
curl -X DELETE 'localhost:8080/api/downloads/<hash>?delete_files=true'
```

## Prowlarr, Sonarr, Radarr

The connector speaks enough of the qBittorrent Web API to be added as a download
client: choose **qBittorrent**, host = the connector, port `8080`, any username and
password, and set the category (`tv`, `movies`, ...).

For Sonarr/Radarr to import finished files, they must see the downloads under the same
path as the connector does (mount the same host folder at `/downloads` in both
containers), or configure a remote path mapping.

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `ALLDEBRID_API_KEY` | required | AllDebrid API key |
| `AD_LISTEN` | `0.0.0.0:8080` | Listen address |
| `AD_DATA_DIR` | `/data` | State file and queued `.torrent` files |
| `AD_DOWNLOAD_DIR` | `/downloads` | Parent of the default category folders |
| `AD_CATEGORY_DIRS` | – | Extra/overridden categories, `name=/path,name=/path` |
| `AD_DELETE_AFTER_DOWNLOAD` | `true` | Remove the magnet from AllDebrid once it is on disk |
| `AD_MAX_DOWNLOADS` | `2` | Items downloaded to disk at the same time |
| `AD_POLL_SECONDS` | `5` | How often AllDebrid is polled |
| `RUST_LOG` | `info` | Log level |

## How it behaves

- Items are queued and sent to AllDebrid as its limit of 30 active magnets allows.
- The queue is saved in `/data/state.json`; after a restart, downloads resume.
- Files are written as `<name>.part`, resumed with HTTP `Range`, size-checked, then renamed.
- Re-adding an item that failed retries it; re-adding anything else is a no-op.

## Layout

- `crates/alldebrid` – async AllDebrid API library (see `SPEC.md`).
- `crates/alldebrid-connector` – this backend.
- `web/` – the optional static web UI and its nginx config.
