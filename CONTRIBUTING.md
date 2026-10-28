# Contributing

1. Create a branch from `main`.
2. Keep scraper tests deterministic and independent of live websites.
3. Run `cargo fmt -- --check` and `cargo test --locked`.
4. Describe behavior changes and any database migration impact in the pull request.

Never include credentials, private fixtures, downloaded media, or local database files in commits.
