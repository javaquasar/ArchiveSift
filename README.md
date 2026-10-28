# ArchiveSift

ArchiveSift is a Rust desktop application for collecting downloadable content into a searchable local archive. The current public adapter downloads images and metadata, stores them in SQLite, and provides filters for source, tags, actors, categories, and studios.

The repository ships with a self-contained demo website and demo scraper. Site adapters can be added by implementing the `ContentScraper` trait and registering the adapter in the scraper registry.

## Features

- URL queue and clipboard monitoring
- Concurrent page processing and image downloads
- SQLite metadata and full text search
- Filters by source, tag, actor, category, and studio
- Local thumbnails and an image preview viewer
- Structured download folders
- Import and export tools, plus a read-only SQL workspace
- Static demo target for development and tests

## Demo download fixture

`demo-site/galleries/random-showcase-004/` contains six local generated PNG
files and every metadata selector supported by the public demo scraper. The
integration test serves this page from a local HTTP server, downloads all files,
creates thumbnails, persists metadata in SQLite, and verifies source and tag
filters without depending on external websites.

The `Demo Pages` workflow publishes `demo-site` to GitHub Pages after changes on
`main`. The `CI` workflow runs unit and integration tests, builds the release
executable, and stores it as a workflow artifact.

## Build and run

Install the stable Rust toolchain, then run:

```powershell
cargo build --release
.\target\release\archive-sift.exe
```

For development:

```powershell
cargo test --locked
cargo test --locked --test demo_site_download
cargo run
```

## Adding a scraper

1. Implement `ContentScraper` in a new module under `src/scraper`.
2. Keep URL matching inside `supports_url`.
3. Return normalized metadata and image URLs from `scrape_content`.
4. Implement `scrape_listing` when the source has listing or search pages.
5. Register the adapter in `register_public_scrapers`.
6. Add parser tests using local HTML fixtures or the included demo site.

The shared application code does not require credentials. Adapters that need authentication should keep credentials outside source control and scope them to the exact destination host.

## Repository layout

- `src/scraper` — scraper interface, registry, and demo adapter
- `src/services` — queue, download, search, and thumbnail services
- `src/db` — SQLite persistence and queries
- `src/ui` — desktop interface
- `demo-site` — safe static pages for parser development

## Security

Do not commit credentials, session data, private fixtures, downloaded media, or local databases. See [SECURITY.md](SECURITY.md) for reporting guidance.
