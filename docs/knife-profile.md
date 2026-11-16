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
destination conflicts. It reads the archive and the configured destination.
The preview makes no writes to the archive or the library.
