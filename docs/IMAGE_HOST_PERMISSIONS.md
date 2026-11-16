# Image host permissions

In **Operations → Activity → Image host permissions**, expand a host to see the
extension, affected cards and resource paths. **Allow host and retry all** saves
the grant and queues the affected cards. **Retry all cards** reuses a saved grant.
Each entry can open its card in the library, and an approved host can be revoked.

Each retry reloads the affected card through its extension to obtain fresh image
URLs. It uses the existing image repair queue, which skips valid local files.
The list refreshes as card jobs finish. If a job has another download error, it
also appears under **Failures**. The grant remains available for a later retry.

## Library storage

| Table | Primary key | Contents |
| --- | --- | --- |
| `image_origin_grants` | `extension_id, origin` | A user's decision for one exact HTTPS origin in this library |
| `image_access_issues` | `page_id, extension_id, origin, resource` | The image or viewer path, affected card, and reason for the block |

`page_id` references `source_pages`. Removing a card removes its issue rows. The
index on `extension_id, origin, page_id` supports grouped retries. Grants can
outlive the issues and can be revoked individually. A failed retry keeps its
issues available. An existing library's image-host notices are imported once
per extension when the extension is enabled. Older notices may contain only the
host name.

The app keeps query strings and fragments out of issue rows. Replaying a card
gets fresh signed URLs from the extension. Grants belong to the library and
apply to later versions of the same extension ID.

## Extension interface

The WASM WIT 1.0 and process DTOs remain compatible. An extension can return an
image URL in `Content.images`. The host validates the URL and records an issue
for an unapproved origin while keeping the rest of the card.

An extension that resolves originals behind image viewers can report an omitted
resource through a content attribute with kind `image_origin_required` and
`source_url` set to the image or viewer URL. The host also recognizes the legacy
`notice` attribute labeled `Image host permission`. The same path can be
reported on multiple cards and remains linked to each of them.

The `state-v1` settings envelope optionally includes `image_origins` as an array
of host-approved origins. Use `unpack_settings_with_image_origins` and match
exact origins when selecting image candidates. To request an image viewer at a
granted origin, tag a bodyless GET continuation with the header
`X-ArchiveSift-Image-Viewer: 1` after the first content fetch. The broker consumes
the marker and makes a credential-free request. It gives the guest a 403 when a
viewer is blocked so the guest can keep a preview if it has one.

Additional grants are restricted to HTTPS DNS names on the default port.
Requests using a grant use a separate client, which resolves only public
addresses and checks the destination of every redirect. The grants apply to
image downloads and marked viewers. Routes, listing requests, API credentials,
and attachment permissions continue to use the package manifest.
