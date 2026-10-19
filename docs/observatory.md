# Observatory

The Observatory is a read-only terminal browser for the memory graph, recorded
scenes and touchstones. Start it on your configured owner or try synthetic data:

```sh
mnemed tui                         # configured read_default; legacy owner if unconfigured
mnemed --store global tui          # deliberate named-store selection
mnemed --store mneme tui           # project store, even from another workspace
mnemed tui --view scenes
mnemed tui --view touchstones
mnemed tui --view config          # configuration/provenance, no automatic memory reads
mnemed tui --demo                  # no service needed
mnemed --remote URL tui            # existing HTTP or SSH owner
mnemed tui --config library.json   # explicit multi-source library view
```

The viewer opens no database files and makes no memory changes. Your selected
server must support identity-guarded graph reads; unsupported views show an error,
not an empty store. `--db` and JSON output are unsupported; use CLI/MCP readers
for scripts.

## Configuration inspector

`--view config` works with zero stores, a missing default, blocked stores or
malformed configuration. It shows the shared effective settings, local overrides,
publishers/branch witnesses, conflicts and admission separately from activation.
Left/right or Tab selects a store; arrows, Page Up/Down and Home/End scroll its
details. Esc closes the startup inspector to the source picker; Enter explicitly
opens the chosen source, including the first one.

Opening this modal does not admit new memory reads. Already in-flight work may
finish. Initial opt-in [discovery](discovery.md) can fetch bounded catalog metadata;
retained receipts need no publisher contact. Explicit remote/library/demo targets
stay isolated from ambient configuration discovery.

## Controls

| Key or action | Effect |
| --- | --- |
| Arrows or `j`/`k` | Select a card; scroll when a detail is open |
| Enter | Open the selected memory, scene or touchstone |
| `s` / `t` | Open Scenes / Touchstones; press again to return to Map |
| `/` | Semantic search in Map; lexical search in Scenes |
| `i` | Toggle Map's text inspector, or open a scene/touchstone detail |
| `e` | Focus the selected memory's connections in Map |
| `h` | Hide/show archived nodes in Map |
| `+` / `=` / `-`, wheel over Map | Zoom the camera without rearranging nodes |
| `f` / `0` | Fit the shown loaded field / restore 100% scale |
| `n` / `m` | Continue paused Map loading; `n` pages Touchstones |
| `r` | Refresh the current view |
| Tab | Choose among Known sources |
| `C` (or `c` in the picker) | Inspect configuration, discovery and displayed read provenance |
| `?` | Help |
| Esc | Close detail/help/focus, pause loading, or go back |
| Backspace / `b` | Go back |
| `q` / Ctrl-C | Quit |

In Map, click a marker or index row to select it. Drag the field to pan;
Shift+arrows pans from the keyboard. Ctrl+Left/Right traverses visible markers by
column; Ctrl+Up/Down by row. `,` / `.` selects among all loaded, non-hidden nodes,
including offscreen ones, and brings selection into view.

`+` (or `=`) / `-` zoom around the center of the field; wheel/trackpad over the
map zooms around the pointer. `f` fits the currently shown loaded field, including
offscreen nodes; it does not load the rest of the store. `0` restores 100% scale.
Zoom changes the camera, not the layout: nodes keep their world positions, while
markers, labels and the selected circle stay readable at terminal-cell size.
Zoom in for a neighborhood, or fit the field for an overview. Text panes keep
their existing wheel-scrolling behavior.

In an open detail, Page Up/Down moves a viewport page, Home/End jumps to the ends,
and wheel/trackpad scrolls over the text pane. Scrolling does not fetch more body
text: the displayed excerpt remains bounded. Map pan/selection shortcuts still
work with a detail open.

<a id="map-the-graph-is-not-a-sixteen-card-sample"></a>

## Map

Map pages lightweight node identities, statuses, tags and stored edges. Summaries
load lazily near the viewport; bodies are not crawled. Missing or failed summaries
remain placeholders. Enter explicitly reads the selected neighborhood and body;
`i` only toggles the text inspector.

During the initial crawl, use the loaded index to select memories. The field
settles when loading finishes, pauses or fails. Later nodes are added without
moving existing ones; selection, panning, zooming and resizing do not re-solve the layout.
Refresh can settle a changed field. Enter opens a neighborhood; Back restores the
previous arrangement and camera, including its zoom.

Lines are stored relationships. Groups summarize only loaded links, ignoring
direction for layout. A label is an existing authored tag shared distinctively
by enough group members; incomplete tag evidence can leave a group unnamed.
Groups are not stored communities or complete database clusters. Position is a
visual aid, not semantic distance. Use the connection inspector for actual edge
direction, type and weight.

Core notes use a gold diamond; archived notes use `×`, gold when also core.
`h` hides/shows archives without deleting them. Relationship rows describe the
neighbor relative to selection: “supersedes this,” “is superseded,” “sources this,”
“is sourced” or “is associated.”

`e` focuses connections without moving retained nodes. It combines known field
links with a bounded neighbor page and marks incomplete coverage. `e` or Esc
restores the field. This lens reads no bodies; Enter does.

`/` opens a separate semantic-search lens with at most 16 hits. Search results
are not the default graph inventory and cannot establish complete store coverage.

### Loading and coverage

Native topology pages contain at most 256 node/edge records. Automatic loading
stops between pages after 64 calls, 8 MiB of wire data or 15 seconds elapsed;
an in-flight read has its own five-second timeout. Retained data has a separate
16 MiB ceiling. `n`/`m` starts another loading burst, subject to that retained-data
limit. Esc pauses; refresh retries failed work.

Loaded and visible counts describe this field, not the database total.
Pages form a best-effort live scan, not an atomic snapshot. Partial loading,
missing summaries and paused work stay visible.

## Scenes: when it happened, when it was recorded

Scenes uses the episode timeline. `o` switches between recording and occurrence
time. Times are UTC. Occurrence order excludes undated scenes; switch to recorded
time to see them. Lexical search can still find an undated scene on either axis.
Occurrence contexts describe where the event happened, separately from its recorder.
Order alone does not establish causation.

The view loads one page of at most 32 scenes and marks additional/incomplete
coverage; it does not crawl later pages. `/` searches summaries lexically; clear
the search to return to the timeline. Search ordering applies only to that bounded
window. For more pages, use [episode CLI reads](episodic-memory.md#find-a-past-event).

Enter or `i` reads the displayed exact edition, with up to 8 KiB of body text.
Refresh retains selection by episode root. An open edition stays pinned even if
a newer one appears; its current-head metadata is shown separately. Close and
reopen to inspect the newer selection. A failed read is not an empty timeline.

<a id="touchstones-a-cabinet-not-a-leaderboard"></a>

## Touchstones

Touchstones preserve an authored explanation of why material matters.
The shelf shows up to eight annotations per page: `n` replaces the page, and `r`
returns to the first. There is no background crawl or search. Switching sources
resets the shelf. Partial coverage, unavailable reads and an empty final page are
reported separately.

Enter or `i` reads an annotation with up to 8 KiB of body text. `[` / `]` or
Left/Right selects one of its historical references. A reference retains its
exact database/node identity, summary, provenance, creation time and memory kind.
It is a **summary-only snapshot**, not an archived body. “Unchanged” compares
those snapshot fields; it says nothing about the body's content.

`c` reads that exact target today. Current summary/body excerpts appear separately
and never replace the historical snapshot or follow a successor. A missing target
cannot recover an archived body; an unavailable owner does not establish deletion.
Esc closes the current read, then the annotation, then returns to Map.

The shelf also respects the 256 KiB protocol-text ceiling. If large encoded notes
make a page unavailable, request a smaller page from the same owner:

```sh
mnemed --remote URL list --touchstones --limit 2
```

Add `--user` when inspecting user memory. See [touchstones](touchstones.md).

## Activity and read sources

Activity glows mean a node was returned in a prepared read response, including
this viewer's own reads. They do not prove that a client received or used the
memory, or that the store changed. The best-effort in-memory feed may lose signals;
restarts and gaps are normal. It retains no query or body text. Older servers may
provide only sampled outstanding work.

Polling does not refresh the graph, scenes or touchstones: use `r`. Reads have
five-second timeouts; activity polls run every two seconds while the serial
network worker is idle. Closing the viewer stops polling. An unavailable owner
leaves the last view visible with an error.

### Switch sources

Tab opens configured Known sources. Select with arrows/`j`/`k`, then Enter or click;
Esc/Tab/`q` cancels. Unrouted entries stay visible with a reason. Opening the picker
does not probe servers; only the chosen route connects. Failed selection preserves
the current view and never falls back to another source.

With shared `memory.toml` configuration, the default follows `read_default`;
`--memory-config PATH` selects another declarative configuration. Any admitted,
readable store can be chosen in the picker or with `--store NAME`, even when it
is inactive for the current workspace. This deliberate read does not activate
background recall, recording or context delivery. Disabled stores, denied read
permissions, profile restrictions and workspace exclusions still apply.

Configured owner-first caches can supply a read-only snapshot when the canonical
owner is unavailable. A persistent **SNAPSHOT · read-only** header stays with that
content, including history and failed refreshes; wider terminals also show its age,
device and generation. `C` shows the exact capture time and canonical identity.
Compositions containing both live/unknown reads and a snapshot are marked mixed,
not passed off as one coherent capture. See [on-use caches](../integrations/library/CACHE.md).

Without declarative configuration, default selection follows
[legacy CLI owner selection](remote-cli.md#default-project-and-user-owners),
including `--user` for the private user owner. `tui --config PATH` instead shows
that library's descriptors, `--remote` one explicit route and `--demo` synthetic
sources; these explicit modes cannot be mixed with declarative selectors. There
is no cross-source graph traversal or automatic snapshot fallback in these explicit modes.

`mnemed stores [--json]` lists the same metadata without opening the viewer.

## Design

The field uses a dark background, restrained mint/lilac light and stable spatial
placement. Longer explanations stay in selection details and help.
For a noninteractive synthetic preview:

```sh
mnemed tui --demo --view scenes --snapshot
```
