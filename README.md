# ArchiveSift

ArchiveSift is a Rust desktop application for collecting downloadable content into a searchable local archive. The current public adapter downloads images and metadata, stores them in SQLite, and provides filters for source, tags, actors, categories, and studios.

The repository ships with a self-contained demo website and demo scraper. External
adapters can be installed as WASM components or trusted native process packages
without rebuilding the application. See [extension architecture and UML](docs/EXTENSION_ARCHITECTURE.md).

## Features

- URL queue and clipboard monitoring
- Concurrent page processing and image downloads
- SQLite metadata and full text search
- Filters by source, tag, actor, category, and studio
- Local thumbnails and an image preview viewer
- Structured download folders
- Import and export tools, plus a read-only SQL workspace
- Static demo target for development and tests
- Dynamic WASM and process extensions with a shared SDK and an Extensions status tab

## Demo download fixture

`demo-site/galleries/random-showcase-004/` contains six local generated PNG
files and every metadata selector supported by the public demo scraper. The
integration test serves this page from a local HTTP server, downloads all files,
creates thumbnails, persists metadata in SQLite, and verifies source and tag
filters without depending on external websites.

To check installation and operation of the production WASM demo parser separately:

```powershell
python scripts/test_extensions.py --wasm-demo-only
```

This builds a component without test-only behaviors, packages it, and runs
`tests/wasm_demo_site.rs` against the actual demo files. The checks verify registry
discovery, WASM selection over the built-in fallback, metadata, channel/model
listings, lazy API pagination, six distinct PNG downloads, SQLite and thumbnails.
Disabled, missing, corrupt and incompatible packages are also checked.

The `Demo Pages` workflow publishes `demo-site` to GitHub Pages after changes on
`main`. The `CI` workflow runs unit and integration tests, builds the release
executable, and stores it as a workflow artifact.

## Build and run

Install the stable Rust toolchain, then run:

```powershell
cargo build --release
.\target\release\archive-sift.exe
```

The full desktop workspace opens on the first launch: **Scrape** contains source
and destination settings, the URL queue and activity log; **Search** provides
image filters and previews; **SQL** opens the database console. **Extensions**
shows installed parser modules. Choose a download folder in Scrape to enable
downloads. Search and SQL remain available without a download folder.

Open **Settings → Storage** to select the download folder, open an existing
SQLite library or create a new one, and optionally choose a separate thumbnail
folder. **Save and apply** validates and saves the selection without restarting.
Applying waits until downloads and database operations are idle. Changing paths
does not move existing files; when opening another library, select both its
database and its download folder. The Settings tab also displays the actual
configuration file path. Local settings remain excluded from Git.

For development:

```powershell
rustup target add wasm32-unknown-unknown
python scripts/test_extensions.py
cargo run
```

## Adding a scraper

Use the versioned `archive-sift-scraper-api` contract and the public example in
`plugins/demo-parser`. Package HTML/API parsers as WASM components; package
browser automation as a trusted native process implementing the same contract.
Install packages under `%APPDATA%\ArchiveSift\plugins` on Windows, or
`$XDG_DATA_HOME/ArchiveSift/plugins` on Linux (falling back to
`$HOME/.local/share/ArchiveSift/plugins`). Set `ARCHIVESIFT_PLUGIN_DIR` to override
the location. Enable the package in its `plugin.json` and restart;
native packages also require `trusted: true`.

Built-in `ContentScraper` registration remains available for demo/system modules.
The [architecture document](docs/EXTENSION_ARCHITECTURE.md) contains installation
commands, manifests, WIT, JSON-RPC examples, trust boundaries and the test matrix.
Its [performance section](docs/EXTENSION_ARCHITECTURE.md#performance-analysis-and-implemented-changes)
records the implemented optimizations, local measurements and the repeatable
`extension_benchmark` example.

The shared application code does not require credentials. Adapters that need authentication should keep credentials outside source control and scope them to the exact destination host.
Host 0.2.0 supports exact-origin/path HTTP header injection from Windows Credential
Manager using local credential bindings. Secrets are never included in WASM
packages. See the architecture document for configuration and template packaging.

## Repository layout

- `src/scraper` — scraper interface, registry, and demo adapter
- `src/scraper/extensions` — dynamic discovery, permissions, WASM and process runtimes
- `crates/scraper-api` — independent protocol SDK and typed WIT contract
- `plugins/demo-parser` — public HTML/API extension compiled for either runtime
- `tools/plugin-packager` — validated component packaging and manifest generation
- `docs/EXTENSION_ARCHITECTURE.md` — architecture and UML diagrams
- `src/services` — queue, download, search, and thumbnail services
- `src/db` — SQLite persistence and queries
- `src/ui` — desktop interface
- `demo-site` — safe static pages for parser development

## Security

Do not commit credentials, session data, private fixtures, downloaded media, or local databases. See [SECURITY.md](SECURITY.md) for reporting guidance.
