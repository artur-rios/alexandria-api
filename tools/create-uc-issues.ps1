$ErrorActionPreference = 'Stop'
Set-Location 'D:\Repositories\alexandria-api'

# UC number -> (title, areaLabel, FR list). UC-37 is the health-check use case (ops).
$useCases = @(
  @{N=1;  T='UC-01: Index library files';             L='area:catalog';    R='FR-FC-01, FR-FC-02, FR-FC-03, FR-FC-04, FR-FC-05, FR-FC-06, FR-FC-07, FR-FC-08, FR-FC-09, FR-FC-24'}
  @{N=2;  T='UC-02: Re-index and refresh the catalog'; L='area:catalog';    R='FR-FC-08, FR-FC-10, FR-FC-11, FR-FC-24'}
  @{N=3;  T='UC-03: Browse and view file metadata';    L='area:catalog';    R='FR-FC-12, FR-FC-13, FR-FC-24'}
  @{N=4;  T='UC-04: Edit file metadata';              L='area:catalog';    R='FR-FC-14, FR-FC-15, FR-FC-16, FR-FC-17, FR-FC-18, FR-FC-24'}
  @{N=5;  T='UC-05: Rename a file';                    L='area:catalog';    R='FR-FC-19, FR-FC-24'}
  @{N=6;  T='UC-06: Soft-delete a file';              L='area:catalog';    R='FR-FC-20, FR-FC-24'}
  @{N=7;  T='UC-07: Restore a soft-deleted file';     L='area:catalog';    R='FR-FC-21, FR-FC-24'}
  @{N=8;  T='UC-08: Hard-purge a file record';         L='area:catalog';    R='FR-FC-22, FR-FC-24, NFR-07'}
  @{N=9;  T='UC-09: Purge a file on disk';            L='area:catalog';    R='FR-FC-23, FR-FC-24'}
  @{N=10; T='UC-10: Create a collection';             L='area:collections';R='FR-CO-01, FR-CO-02, FR-FC-24'}
  @{N=11; T='UC-11: Rename a collection';             L='area:collections';R='FR-CO-03, FR-FC-24'}
  @{N=12; T='UC-12: Delete a collection';             L='area:collections';R='FR-CO-04, FR-FC-24'}
  @{N=13; T='UC-13: Add items to a collection';       L='area:collections';R='FR-CO-05, FR-FC-24'}
  @{N=14; T='UC-14: Remove and list items in a collection'; L='area:collections';R='FR-CO-06, FR-CO-07, FR-FC-24'}
  @{N=15; T='UC-15: Create a bookmark';               L='area:bookmarks';  R='FR-BM-01, FR-FC-24'}
  @{N=16; T='UC-16: Update a bookmark';              L='area:bookmarks';  R='FR-BM-02, FR-FC-24'}
  @{N=17; T='UC-17: Browse bookmarks';                L='area:bookmarks';  R='FR-BM-06, FR-FC-24'}
  @{N=18; T='UC-18: Soft-delete and restore a bookmark'; L='area:bookmarks'; R='FR-BM-03, FR-BM-05, FR-FC-24'}
  @{N=19; T='UC-19: Hard-purge a bookmark';           L='area:bookmarks';  R='FR-BM-04, FR-FC-24'}
  @{N=20; T='UC-20: Create a watchlist';              L='area:watchlists'; R='FR-WL-01, FR-FC-24'}
  @{N=21; T='UC-21: Browse watchlists and progress';  L='area:watchlists'; R='FR-WL-08, FR-FC-24'}
  @{N=22; T='UC-22: Add a video to a watchlist';      L='area:watchlists'; R='FR-WL-02, FR-WL-03, FR-FC-24'}
  @{N=23; T='UC-23: Update watch progress';           L='area:watchlists'; R='FR-WL-04, FR-WL-05, FR-FC-24'}
  @{N=24; T='UC-24: Remove a video from a watchlist'; L='area:watchlists'; R='FR-WL-06, FR-FC-24'}
  @{N=25; T='UC-25: Delete a watchlist';              L='area:watchlists'; R='FR-WL-07, FR-FC-24'}
  @{N=26; T='UC-26: Create a reading list';          L='area:reading-lists';R='FR-RL-01, FR-FC-24'}
  @{N=27; T='UC-27: Browse reading lists and progress'; L='area:reading-lists';R='FR-RL-08, FR-FC-24'}
  @{N=28; T='UC-28: Add an item to a reading list';   L='area:reading-lists';R='FR-RL-02, FR-RL-03, FR-FC-24'}
  @{N=29; T='UC-29: Update reading progress';        L='area:reading-lists';R='FR-RL-04, FR-RL-05, FR-FC-24'}
  @{N=30; T='UC-30: Remove an item from a reading list'; L='area:reading-lists';R='FR-RL-06, FR-FC-24'}
  @{N=31; T='UC-31: Delete a reading list';          L='area:reading-lists';R='FR-RL-07, FR-FC-24'}
  @{N=32; T='UC-32: Read text file content';         L='area:text';       R='FR-TX-01, FR-FC-24'}
  @{N=33; T='UC-33: Edit text file content';         L='area:text';       R='FR-TX-02, FR-TX-03, FR-FC-24'}
  @{N=34; T='UC-34: Local login';                    L='area:auth';       R='FR-AU-01, FR-AU-04, FR-AU-07, FR-AU-08'}
  @{N=35; T='UC-35: Set or change local login credentials'; L='area:auth'; R='FR-AU-05, FR-AU-06, FR-AU-08'}
  @{N=36; T='UC-36: Authenticate via external JWT';  L='area:auth';       R='FR-AU-01, FR-AU-02, FR-AU-03, FR-AU-07, FR-AU-08'}
  @{N=37; T='UC-37: Health check';                   L='area:ops';        R='IR-03, IR-04, IR-05'}
)

$ucDir = 'docs\requirements\Use Case Specification Document.md'
$nc = '{0:D2}' -f 0

foreach ($uc in $useCases) {
  $id = 'UC-{0:D2}' -f $uc.N
  $body = @"
Implements **$id** from the [Use Case Specification Document]($ucDir).

## Traced requirements

$($uc.R)

## Acceptance criteria

- [ ] Main flow implemented in `alexandria-core` (Command/Query handler) and covered by unit tests.
- [ ] Every alternative flow (`AF-xx`) for this use case implemented and tested.
- [ ] HTTP/REST-JSON endpoint (where applicable) implemented in `alexandria-http` with an integration test asserting response **and** persisted state.
- [ ] FFI entry point (where applicable) implemented in `alexandria-ffi` with an integration test.
- [ ] HTTP / FFI parity assertion present and passing.
- [ ] Repository / auth-service collaborators faked in unit tests; real SQLite + temp filesystem in integration tests.
- [ ] `cargo test --workspace` is green.
- [ ] Branch `feature/$($id.ToLower())-<slug>` was reviewed by a human and merged.

## Delivery workflow

Follow the pause-gated flow in the [Development Workflow Document](docs/requirements/Development%20Workflow%20Document.md) — branch from `main`, pause before Testing, after Testing, before the PR, and before Done.

## References

- [Use Case Specification Document]($ucDir) — actors, pre/postconditions, main flow, alternative flows.
- [System Requirements Document](docs/requirements/System%20Requirements%20Document.md) — the FR requirements traced to this use case plus the data model.
- [Technology Stack Document](docs/requirements/Technology%20Stack%20Document.md) — technologies and versions.
- [Testing Specification Document](docs/requirements/Testing%20Specification%20Document.md) — how tests are written.
"@
  $url = gh issue create --title $uc.T --body $body --label 'use-case' --label $uc.L --project 'Alexandria API'
  Write-Output ("{0}  ->  {1}" -f $uc.T, $url)
}