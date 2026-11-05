# Local agent access and MCP

ArchiveSift provides `archive-sift-mcp`, a stdio MCP adapter for an already running
desktop application. It implements the `2025-11-25` lifecycle and also negotiates
`2025-06-18`. The adapter does not launch the GUI, create a download engine, open
the library database, change application settings, or install an agent-host config.

## Enable and connect

1. Open **Settings** and enable **Allow agents to use this application**.
2. Read access is granted first. Enable **Allow queue changes and downloads** only
   when the agent should enqueue URLs or control queue dispatch.
3. Click **Copy session token** once and put it in the agent host's secret facility
   as `ARCHIVESIFT_CONTROL_TOKEN`. The application stores only its hash in memory;
   the copied token is cleared from the settings panel. Do not put it in command
   arguments, the public repository, diagnostics or shared configuration.
4. Click **Copy MCP configuration**, or configure the adapter as shown below.

Both switches are session-scoped and start disabled after every application
restart. They are not JSON configuration fields. No MCP tool can enable access,
grant itself downloads, change destinations, read credentials or rotate tokens.
**Create new token** revokes the previous session token. After re-enabling or
rotating the token, update the agent host's secret and restart/reconnect its MCP
adapter so that it receives the new environment value. Disable the main switch
to stop new requests. Running downloads can finish; accepted queued URLs remain
in the application's ordinary queue. Disabling is not a rollback of completed work.
Changing the selected database or destination revokes the grant and requires a
new enable action. Applying storage or JSON configuration revokes it as well.

Example agent-host configuration (use absolute installed paths):

```json
{
  "mcpServers": {
    "archivesift": {
      "command": "C:/Apps/ArchiveSift/archive-sift-mcp.exe",
      "args": ["--config", "C:/Apps/ArchiveSift/data/app-config.json"]
    }
  }
}
```

Set the token separately in the host's environment/secret facility; the copied
configuration contains no token. On Linux, use the installed `archive-sift-mcp`
executable and the absolute path of the same application profile. The `--config`
argument identifies the application's JSON settings file, not a download folder.
It is used only to locate the local endpoint; the adapter does not load its secrets.

Build both executables together with:

```text
cargo build --locked --release --bins
```

Keep `archive-sift` and `archive-sift-mcp` in the same directory. Windows CI includes
both executables and this guide in its application artifact. `--help` and
`--version` are local diagnostic modes; normal adapter stdout is JSON-RPC only.

## Tools

| Tool | Permission | Result |
| --- | --- | --- |
| `archivesift_status` | Read | Application version, grants and queue counts. |
| `archivesift_extensions` | Read | Installed parser IDs, versions, runtime and loaded state. |
| `archivesift_search_images` | Read | Image/page IDs, titles, source links, favorite flags and stable pagination cursor. |
| `archivesift_search_torrents` | Read | Compact torrent cards with counts and offset pagination. |
| `archivesift_torrent_details` | Read | Bounded titles, descriptions, credits, image IDs and attachment names. |
| `archivesift_queue_status` | Read | Counts, current work and a bounded pending preview. |
| `archivesift_queue_add` | Downloads | Add supported URLs to the same persistent queue used by the UI. |
| `archivesift_queue_pause` | Downloads | Pause dispatch of subsequent items; active downloads can finish. |
| `archivesift_queue_resume` | Downloads | Resume dispatch using the application's settings and parser registry. |
| `archivesift_queue_retry` | Downloads | Retry failed queue items. |

Read the input schemas from `tools/list`; unknown fields and out-of-range values
are rejected. Queue acceptance is not download completion: poll queue state to
observe subsequent processing. Duplicate URL submissions use the ordinary queue's
deduplication rules. Requests cannot select a different database or output path.
There are no tools for arbitrary SQL, shell execution, file removal, folder moves,
credential readout, settings edits or extension installation. Folder changes remain
in the existing UI confirmation flow.

Library titles, descriptions and other metadata are untrusted data, not agent
instructions. Configuration and credential-store values are never returned.
Known credential query parameters and URL user information are removed from link
projections. Filesystem paths and raw OS/SQLite errors are not exposed. Authorized
activity is logged using the tool name and outcome, without raw arguments or tokens.
Runtime-loaded private parser identities remain local data; the public host contains
only the generic interface and synthetic test fixtures.

## Transport and lifecycle

```mermaid
sequenceDiagram
    participant U as User / Settings
    participant A as Agent MCP client
    participant M as archive-sift-mcp
    participant C as Local control service
    participant S as Existing application services
    U->>C: Enable session; issue token and grants
    A->>M: initialize / notifications/initialized
    A->>M: tools/list / tools/call
    M->>C: Authenticated bounded local request
    C->>C: Check instance, token, grant and input schema
    C->>S: Read selected library or use shared URL queue
    S-->>C: Result or queue acceptance
    C-->>M: Bounded structured response
    M-->>A: MCP content + structuredContent / isError
    U->>C: Disable or rotate token
    C->>C: Revoke access and cancel active observations
```

The internal application connection is newline-framed JSON over an ephemeral
`127.0.0.1` TCP listener, authenticated with an OS-random 256-bit session token.
It is not an HTTP MCP endpoint and does not listen on external interfaces. The
stdio adapter discovers it through `<config-file>.agent-endpoint.json`, containing
only a protocol version, instance ID and loopback address. The profile lock prevents
two enabled control services for the same canonical config file. A new application
session has a new token and instance ID; stale clients cannot reuse their grant.

The settings/profile directory must be private to the OS user. On Unix, writable
group/world profile directories are rejected and discovery records are created
with mode 0600. Windows uses the profile directory's inherited ACL. This feature
is not an OS sandbox against malicious code already running as the same user.

Limits: eight concurrent connections/observations, 64 KiB requests, 256 KiB response
frames, at most 50 images per page, 40 torrent summaries per page and 16 URLs per
enqueue call. Idle reads and stalled writes time out. Active library observations
receive cancellation after 15 seconds or when access is revoked. A dispatched
queue mutation may complete even if the adapter disconnects. The network listener
and request workers do not run on the UI thread. Enqueued work wakes the existing
UI-owned processor rather than constructing another downloader.

## Validation

`cargo test --locked --test agent_control` exercises the real adapter executable,
MCP lifecycle, tool schemas, authenticated local IPC, read/download grants,
revocation, profile exclusivity, token rotation, pagination, queue deduplication,
input limits, malformed requests, endpoint restrictions and response redaction.

`cargo test --locked --lib agent` covers headless Settings toggle interactions,
cancelled startup, storage-change revocation, bounded output and observation
cancellation. The Windows workspace job and Linux agent-control job run these checks.
All tests use temporary profiles and synthetic local metadata, with no real logins
or external downloads.

Protocol references: [stdio transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports),
[lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle),
[tool calls](https://modelcontextprotocol.io/specification/2025-11-25/server/tools).
