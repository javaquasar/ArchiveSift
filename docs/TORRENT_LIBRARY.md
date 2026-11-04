# Torrent cards and studio folders

Search has separate Images and Torrents modes. Torrent cards include attachment-only
pages, so a missing poster does not hide a release. The torrent list supports text,
source, studio, year, tag and category filters, with 40 cards per page.

New torrent pages use the following layout, relative to the configured effective
download folder (including its optional subfolder):

```text
torrents/<source>/<primary studio>/<content title> [topic ID-URL hash]/
    <attachment>.torrent
    <reference>.magnet.txt
    description.txt
    images/
```

Each new or reprocessed torrent card exports its complete page heading and description
to `description.txt` in UTF-8. Paragraphs and Unicode are preserved without truncation.
The full first-post text (`full_description`) takes precedence; `description` is the
fallback. If the parser supplies neither, the file contains the heading alone.
The heading is the original page title, independent of the shorter folder name.

The export is written atomically before asset downloads, so available text is retained
even if a torrent requires a new session. Reprocessing refreshes the same tracked
file; deleting it and reprocessing recreates it. This is a generated file: keep manual
notes in a separately named file. An untracked, conflicting `description.txt` is
preserved and reported instead of being overwritten. The export appears under Files
on the card and moves with its images, torrent and magnet. Existing cards receive it
when processed again; there is no automatic library-wide backfill.

There is no year directory. Dates and years remain searchable metadata. The URL
hash distinguishes topics with the same visible identifier; portable components
handle Unicode, forbidden characters, Windows device names and trailing dots.
All generated paths use the platform's native separators.

## Editing

Open Search → Torrents and select a card. Choose a primary studio, or add a new
studio and select it. Add/remove tags and categories using their editors; existing
values are suggested as you type. Tags and categories are independent, multiple
values per card, and do not affect the folder structure.

Each image has a Main image checkbox, also available in the enlarged preview.
Only one explicit choice is allowed per card. Selecting it updates the large preview
immediately; Review and save and the confirmation persist it with the other edits.
Clearing the choice uses the first image automatically. The choice is scoped to the
card, survives reprocessing, restarts and studio moves, and resets to the automatic
fallback when the selected image is deleted. Changing only the main image moves no
files. Existing cards use the first image until a main image is chosen.

When a parser upgrades a stored preview to its original, the page links only to the
original instead of showing both versions. Favorites, image tags and the selected
main image follow the upgrade. References from other pages and existing files are
retained. The original gets its own download path, so an existing thumbnail cannot
be mistaken for a completed original download.
Rotated signed download URLs for the same resource path and stable preview replace
their earlier page link as well. Different resources and explicitly requested image
variants are retained separately.

Review and save prepares a preview containing the source locations, destination,
file count and size. Nothing moves until Confirm and move. Cancel retains the
draft without writing it. Metadata-only confirmation keeps current file locations.

The complete parser-provided studio list is retained. A separate primary selection
defaults to the first studio emitted by the parser, with an unknown-studio fallback.
Manual additions and removal overrides survive subsequent scrapes. Newly discovered
parser tags can still appear; removing one existing tag does not freeze the entire
tag set. The selected primary studio is also visible in image search results.

## Existing libraries

Schema initialization discovers existing attachment and magnet pages without moving
their files. Their directory stays unset until the user changes the primary studio
or checks Organize existing files into a studio folder and confirms. Only files
belonging to that legacy card are gathered. Existing managed card directories include
their user-added sidecars when moved. Thumbnail caches stay in their configured
cache folder; page-image and attachment paths are updated transactionally.

Reprocessing a legacy card writes `description.txt` next to its existing torrent or
magnet, without moving the assets. The tracked export is included when the user later
confirms organization into a studio folder.

## Transfer and recovery

```mermaid
sequenceDiagram
    actor User
    participant UI as Card editor
    participant FS as File worker
    participant DB as SQLite
    User->>UI: Review changes
    UI->>FS: Prepare plan and SHA-256 fingerprints
    FS-->>UI: Old/new folders, count, size
    User->>UI: Confirm
    UI->>FS: Apply confirmed plan
    FS->>DB: Persist prepared journal + card revision
    FS->>FS: Rename directory or move/copy verified files
    FS->>DB: Commit paths, metadata, revision and journal phase
    FS->>FS: Clean up verified originals and empty folders
    FS->>DB: Remove journal
    FS-->>UI: Updated card
```

Operations run in background workers and are serialized per source page, together
with that page's downloads. Downloads of unrelated pages can continue in parallel.
An operating-system file lease prevents a second application instance from recovering
an active move. The lease is released automatically if its process exits.
Revisions reject stale editor confirmations. Existing destinations and changed file
contents are rejected before transfer, and a failed metadata commit rolls files back.

Same-volume managed directories use rename; cross-volume transfers stream through
an exclusive temporary file, sync it, and verify SHA-256 before finalization. Copies
retain their originals until the database commit. A file referenced by another page
is copied and its original retained. The application never starts a torrent client.

The SQLite journal records prepared and committed phases. Startup, opening the torrent
list and subsequent downloads recover interrupted work: prepared moves roll back,
committed moves finish cleanup. Recovery is idempotent. Changed files or a different
storage root retain the journal and report an error instead of discarding data.
Path traversal and symbolic links outside the archive are rejected.

## Validation

`cargo test --locked --test torrent_library` covers layouts, legacy organization,
manual metadata, filters, stale plans, collisions, failed commits and shared files.
`cargo test --locked --lib torrent` includes interrupted renames/copies, cleanup,
concurrent confirmations, migrations and headless UI confirmation/cancellation tests.
Attachment integration tests also reparse a moved card and verify it reuses files.
`cargo test --locked --test torrent_description` checks full text exports, Unicode,
fallback descriptions, reprocessing, legacy moves, user-file conflicts, failed
database writes and atomic replacement failures.
CI runs these checks on Windows and Linux. Tests use synthetic local data only.
