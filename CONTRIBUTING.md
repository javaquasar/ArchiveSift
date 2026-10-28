# Contributing

1. Create a branch from `main`.
2. Keep scraper tests deterministic and independent of live websites.
3. Install `wasm32-unknown-unknown` using `rustup target add wasm32-unknown-unknown`.
4. Run `cargo fmt --all -- --check` and `python scripts/test_extensions.py`.
   The script prepares real WASM/process fixtures and runs `cargo test --locked --workspace`.
5. Describe behavior changes and any database migration impact in the pull request.

Never include credentials, private fixtures, downloaded media, or local database files in commits.
