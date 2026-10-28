# Demo Site

This folder contains a small static website that can be published with GitHub Pages
and used as a safe scraping target for:

- demo scraper development
- parser regression tests
- listing-page pagination tests
- lazy-loading payload extraction tests
- public examples that should not expose private site logic
- end-to-end download tests using local generated PNG files

## Entry Point

- `demo-site/index.html`

## Useful Stable Selectors

- `[data-demo-role="listing"]`
- `.demo-gallery-link`
- `.demo-next-page`
- `[data-demo-lazy-source]`
- `[data-demo-role="gallery"]`
- `[data-demo-title]`
- `[data-demo-actor]`
- `[data-demo-tag]`
- `[data-demo-category]`
- `[data-demo-studio]`
- `[data-demo-full]`
- `[data-demo-description]`
- `[data-demo-date]`
- `[data-demo-director]`
- `[data-demo-duration]`
- `[data-demo-studio-code]`
- `[data-demo-resource-link]`

## Suggested GitHub Pages Layout

If this repository is published directly with GitHub Pages, this folder can be used as:

- `/demo-site/`

If it is moved to a separate repository, publish the folder contents as the site root.

## Lazy Listing Fixture

The page:

- `demo-site/channels/studio-nova/lazy/index.html`

simulates client-side gallery tile loading through:

- `demo-site/api/lazy/studio-nova-page-1.json`
- `demo-site/api/lazy/studio-nova-page-2.json`

The demo scraper is expected to inspect `data-demo-lazy-source`, fetch the JSON
payload chain, and extract gallery links from each payload item.

## Random Download Fixture

`galleries/random-showcase-004/` contains every metadata selector supported by
the public demo scraper and six local PNG files. Browser JavaScript shuffles the
cards for presentation, while the underlying URLs remain deterministic for tests.

Regenerate the PNG fixtures with:

```sh
python demo-site/generate_random_images.py
```
