# Knife profile

ArchiveSift recognizes a knife through the existing scraper API. Return the
page title and ordered images in the usual content response, and supply these
page attributes.

| Kind | Value |
| --- | --- |
| `catalog_profile` | `knife` |
| `manufacturer`, `model` | Required. Use the base model name across variants. |
| `steel`, `handle_material`, `model_number`, `sku` | Optional variant details. |
| `category`, `description`, `technical_details` | Optional display and search metadata. |
| `blade_length_mm`, `weight_g` | Optional metric measurements. |
| `price`, `price_currency`, `availability`, `canonical_url` | Optional details for the current source page. |

The host groups source pages for the same manufacturer and model in SQLite.
Variant details stay on their source pages. Files go under
`knife/<manufacturer>/<model>` inside the effective download folder. The
default filename is `<manufacturer> - <model> - <steel> - <number>.<ext>`.
The host reserves paths for each page and image. Collisions get a numeric
suffix. Retries reuse existing assignments. The host owns database migration,
path validation and downloads.

Under **Settings → Import**, open a legacy `knives.json` next to its
`images` directory. The preview streams the index and shows destination
folders, existing filenames, missing files, invalid paths, repeated IDs and
destination conflicts. Select **Review and import…** and confirm **Import** to
copy available local photos into the library using the original filenames.
The import checks archive IDs, source URLs and file contents. It also matches
different sizes of the same source image. Matching pages keep their current
metadata and receive missing archive fields. Conflicting files stay in place
and are counted in the import results.

Under **Search → Filters → Manufacturers**, select a manufacturer from
the suggestions and apply the filters. This filter combines with text, site,
tags and the other image search filters.

In the Search source menu, choose **Knives (group)** to search across every
source with saved knife pages, including imported records. Individual sites
remain available in the same menu. The group also applies when browsing by
date. New groups appear from the saved `catalog_profile` metadata, so adapters
can add or reuse a group through the existing scraper API.
