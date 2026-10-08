# Contributing

Reading order for a new contributor: `docs/initial/Project Overview.md` →
`docs/requirements/Vision Document.md` →
`docs/requirements/Use Case Specification Document.md` →
`docs/System Behavior Document.md`.

## Repository layout

```txt
alexandria-api/
├── Cargo.toml                 # workspace
├── crates/
│   ├── alexandria-core/       # domain: commands, queries, repos, auth, config
│   ├── alexandria-http/       # axum routes + middleware
│   └── alexandria-ffi/        # extern "C" + cbindgen header
├── config.toml.example
├── docs/
│   ├── initial/               # informal docs (Project Overview, Stack, Workflow, Business Rules)
│   ├── requirements/          # formal specs (Vision, SRD, Use Cases, …)
│   └── System Behavior Document.md   # how the running system behaves, with diagrams
├── tools/
├── CHANGELOG.md
├── CONTRIBUTING.md
└── README.md
```

## Prerequisites

Requirements: Rust **1.94** or newer (edition 2021) and `cargo`. The floor comes
from sqlx 0.9, the highest MSRV in the dependency graph.

`alexandria-core` links against `ffmpeg-next` for video metadata extraction, so
the ffmpeg C development libraries and `clang` (for bindgen) must be installed
locally before the workspace will build — the only system dependency this
project has. Without them `cargo build` and `cargo test` fail for the whole
workspace, not just the video code, so install them first.

Any ffmpeg from **3.0 to 9.0** works — `ffmpeg-sys-next 9` gates its bindings
by version and covers that whole range — so on a platform with a system package
you can install whatever it offers. CI builds against Ubuntu's 6.1.

### Debian/Ubuntu

```bash
sudo apt-get install libavformat-dev libavcodec-dev libavutil-dev libavfilter-dev libavdevice-dev libswscale-dev libswresample-dev pkg-config clang
```

### macOS

```bash
brew install ffmpeg pkg-config llvm
```

Homebrew's `ffmpeg` formula tracks the current major, which is inside the
supported range.

### Windows

Windows has no system package that ffmpeg's build tooling finds automatically,
so this takes a few deliberate steps. One constraint causes nearly every failed
attempt, so read it before picking an option.

**The build needs ffmpeg's headers and import libraries, not `ffmpeg.exe`.**
Most Windows ffmpeg downloads — including everything labelled "essentials", and
the packages you get by searching for "ffmpeg windows" — ship only a `bin/`
directory with the executables. Those are useless here. You need a build that
also ships `include/` and `lib/`, which in BtbN's naming means a **`.Shared.`**
variant, and in the ffmpeg world generally is called a *dev* or *shared* build.

Version is not a constraint: `ffmpeg-sys-next 9` supports ffmpeg 3.0 through
9.0, so any current build will do.

Prerequisites for every option below:

- **MSVC build tools** — the Visual Studio "Desktop development with C++"
  workload, matching Rust's `x86_64-pc-windows-msvc` target.
- **LLVM/clang**, which `bindgen` needs to parse ffmpeg's headers:

  ```bash
  winget install LLVM.LLVM
  ```

  A default LLVM install is normally enough — bindgen locates `libclang.dll`
  without help (verified against LLVM 22 installed this way). Only if it
  reports that it cannot find libclang do you need to point at it explicitly:

  ```bash
  setx LIBCLANG_PATH "C:\Program Files\LLVM\bin"
  ```

`setx` writes a persistent user variable but does **not** affect the shell you
type it in. Open a new terminal before building. Never use `setx PATH` — it
truncates `PATH` at 1024 characters and can destroy it; edit `PATH` through
Settings → *Edit environment variables for your account* instead.

#### Option A — winget, prebuilt (fastest; ~1 minute)

Best if you just want the workspace building. The trade-off is licensing: BtbN's
packages are GPL builds (see [Licensing](#a-note-on-ffmpeg-licensing) below).

1. Install a **shared** build. Pinning a release branch rather than `master`
   keeps the toolchain reproducible; any of the release branches work:

   ```bash
   winget install BtbN.FFmpeg.GPL.Shared.7.1
   ```

   `winget search ffmpeg` lists the alternatives. Avoid `Gyan.FFmpeg` unless you
   confirm the package ships `include/` and `lib/` — its widely-mirrored builds
   are executables only.
2. Find where winget put it — the directory name contains a hash, so it must be
   looked up rather than guessed. In PowerShell:

   ```powershell
   Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages" -Recurse -Depth 3 -Directory -Filter include | Where-Object { $_.FullName -like "*ffmpeg*" }
   ```

3. Confirm the parent of that `include` directory also contains `lib` and `bin`.
   That parent is your ffmpeg root. If there is no `include`, you installed a
   non-`Shared` variant — go back to step 1.
4. Point the build at it, substituting the path from step 3:

   ```bash
   setx FFMPEG_DIR "C:\path\to\ffmpeg-n7.1-...-shared"
   ```

5. Add that root's `bin` directory to `PATH` (via Settings, per the warning
   above). **Do not skip this.** It is needed at **run** time rather than build
   time, so the symptom is confusing: `cargo build` succeeds, then every test
   binary dies instantly with `exit code: 0xc0000135, STATUS_DLL_NOT_FOUND`,
   because a shared build resolves its DLLs when the process starts.
6. Open a new terminal and jump to [Verifying](#verifying-the-toolchain).

#### Option B — vcpkg (slower; LGPL by default)

Preferred if you care about the licensing of what you link, since vcpkg's
default ffmpeg port is LGPL — it omits the GPL-only codecs. It builds from
source, so budget 30–60 minutes on first install.

1. Clone and bootstrap vcpkg:

   ```bash
   git clone https://github.com/microsoft/vcpkg C:\vcpkg
   C:\vcpkg\bootstrap-vcpkg.bat
   ```

2. Install ffmpeg, pinning a supported major version:

   ```bash
   C:\vcpkg\vcpkg.exe install "ffmpeg[core,avcodec,avformat,avfilter,avdevice,swscale,swresample]:x64-windows"
   ```

3. Tell the build where the tree is — `ffmpeg-sys-next` looks for `VCPKG_ROOT`
   as its second discovery method, after `FFMPEG_DIR`:

   ```bash
   setx VCPKG_ROOT "C:\vcpkg"
   ```

4. The `x64-windows` triplet is a dynamic (DLL) build, and the `vcpkg` crate
   ignores dynamic libraries unless told otherwise:

   ```bash
   setx VCPKGRS_DYNAMIC "1"
   ```

   To avoid this and the runtime DLL question entirely, use
   `:x64-windows-static-md` in step 2 and skip this step.
5. Open a new terminal and jump to [Verifying](#verifying-the-toolchain).

#### Option C — a downloaded build, placed by hand

Equivalent to Option A without winget; use it when you want a specific build.
Download a **shared** ffmpeg 6.1 or 7.1 archive, extract it somewhere stable
such as `C:\ffmpeg`, confirm that directory contains `include/`, `lib/`, and
`bin/`, then:

```bash
setx FFMPEG_DIR "C:\ffmpeg"
```

Add `C:\ffmpeg\bin` to `PATH` for the runtime DLLs, open a new terminal, and
verify.

#### Verifying the toolchain

Build the one crate that links ffmpeg, before the whole workspace — its failure
messages are the legible ones:

```bash
cargo build -p alexandria-core
```

Then confirm the libraries also resolve at run time, which a build alone does
not prove:

```bash
cargo test -p alexandria-core --test hashing
```

#### When it still fails

`ffmpeg-sys-next` reports which discovery methods it tried, in order:
`FFMPEG_DIR`, then vcpkg, then `pkg-config`. Read that list in the error — it
tells you which step above did not take effect.

| Symptom | Cause and fix |
| --- | --- |
| `1. FFMPEG_DIR environment variable (not set)` | `setx` does not affect the current shell. Open a new terminal. |
| `2. vcpkg package manager (ffmpeg package not found)` | `VCPKG_ROOT` unset or the port is not installed for the `x64-windows` triplet. |
| `The pkg-config command could not be found` | Expected on Windows and harmless — it is only the third fallback. If you see it, the real failure is that methods 1 and 2 both missed. |
| `Unable to find libclang` | `LIBCLANG_PATH` is unset or wrong. It must point at the directory containing `libclang.dll`, normally `C:\Program Files\LLVM\bin`. |
| Compile or link errors inside `ffmpeg-sys-next` | Usually a partial install — headers present but import libraries missing, or a mix of two ffmpeg versions on `PATH`/`FFMPEG_DIR`. Confirm one root holds `include/`, `lib/`, and `bin/` together. |
| Builds fine, but tests fail to start or exit with `0xc0000135` | The shared build's DLLs are not on `PATH`. Add the ffmpeg `bin` directory. |

#### A note on ffmpeg licensing

Alexandria is GPL-3.0-or-later (see [LICENSE](LICENSE)), so linking either
ffmpeg variant is lawful. This note used to say the opposite, because the
project was proprietary and only an LGPL build could be linked at all.

The distinction still matters for a different reason. ffmpeg is LGPL by
default; builds configured with `--enable-gpl` — every BtbN `GPL` package
above, and most convenience builds — are GPL, and what they add over the LGPL
build is encoders: x264, x265, xvid. Nothing in Alexandria re-encodes, so an
LGPL build gives the project every capability it actually uses. Prefer it for
anything shipped, and treat a GPL build as a development convenience.

What the desktop front end ships on Linux is a separate matter: it carries
libmpv, which is copyleft wherever it comes from, so those packages are GPL
regardless of which ffmpeg is in them. Flagging the distinction, not giving
legal advice.

## Building

```bash
# Build the whole workspace (core + http + ffi)
cargo build --workspace --release

# Build the HTTP server binary
cargo build --release -p alexandria-http

# Build the FFI dynamic library + regenerate the C header via cbindgen
cargo build --release -p alexandria-ffi
```

The workspace enforces `#![deny(unsafe_code)]` in every crate.

## Testing

Tests are organized by crate and split into **unit** (handler logic against
trait fakes), **integration** (HTTP/FFI end-to-end against real SQLite and a
temp filesystem), and **parity** (HTTP vs FFI must return identical results).
See [`docs/requirements/Testing Specification Document.md`](docs/requirements/Testing%20Specification%20Document.md)
for the full standard.

```bash
# Run the entire suite
cargo test --workspace

# Run only unit tests
cargo test --workspace --lib

# Run only integration tests
cargo test --workspace --test '*'

# Run the HTTP / FFI parity suite alone
cargo test -p alexandria-ffi --test parity

# Optional: line/branch coverage
cargo tarpaulin --workspace --out Html
```

Every use case is delivered with its tests in the same change, per the
[Development Workflow Document](docs/requirements/Development%20Workflow%20Document.md).

CI also checks formatting and lints with every warning treated as an error, so
run these before opening a pull request:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Migrations

Migrations live in `crates/alexandria-core/migrations` and are applied on
startup by both the server and the FFI. Once a migration has been applied
anywhere it is frozen; corrections go in a new migration. Until the first
packaged release, the baseline migrations (`00000000000001_catalog.sql`,
`00000000000011_catalog_runs.sql`) may be amended in place instead, and every
such amendment must be announced as a breaking change in the README's
[Upgrading](./README.md#upgrading) section. See
[Operations & Infrastructure Document §2.5](docs/requirements/Operations%20&%20Infrastructure%20Document.md)
for the full rule.

## Branching model

```txt
feature/<name> ─┐
fix/<name> ─────┴─▶ develop ──▶ release/x.y.z ──▶ main  (tag vx.y.z)
```

| Branch | Cut from | Merges into | How |
| --- | --- | --- | --- |
| `feature/<name>`, `fix/<name>` | `develop` | `develop` | Pull request, merge commit or squash. The branch is deleted on merge. |
| `release/x.y.z` | `develop` | `main` | Pull request, merge commit only. |
| `develop`, `main` | — | — | Protected: no direct pushes, no force pushes, no deletion. |

`develop` is the default branch and where all work lands; `main` holds only
what has been released. Branch names are lowercase: letters, digits, `.`, `_`
and `-`. A `release/` branch is a snapshot of `develop` and carries no commits
of its own: a fix for a release lands on `develop` through a `fix/` branch and a
new release branch is cut.

Every use case is implemented on its own branch, created from an up-to-date
`develop` and named `feature/uc-##-use-case-name` (for example
`feature/uc-01-index-library-files`): one use case = one branch = one issue =
one pull request. The full flow — issue status transitions, the testing gate,
and the Definition of Done — is in the
[Development Workflow Document](docs/requirements/Development%20Workflow%20Document.md).

A pull request into `develop` or `main` needs a green `fmt, clippy, test` check
and a green `branch-policy` check before it can be merged. The **Branch Policy**
workflow ([`branch-policy.yml`](.github/workflows/branch-policy.yml)) enforces
the model above: into `develop` only from `feature/` or `fix/` branches cut from
`develop`, into `main` only from a `release/<major>.<minor>.<patch>` branch whose
every commit is already on `develop` and whose version has no tag yet. Tags
matching `v*` cannot be created, moved, or deleted except by the repository
owner. The owner can bypass these rules; that is for emergencies, not for
routine work.

## Commits and the changelog

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/) with a lowercase subject, e.g.
`feat: browse a folder as a library` or `fix: keep a library to the folder it was actually given`.

Record every change a client or operator would notice under `## [Unreleased]` in
[CHANGELOG.md](./CHANGELOG.md), in the same pull request that makes it.

## Versioning

The project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Alexandria API is a service and a library at once — an HTTP server and the core
the desktop front-end links in process — so its contract is the HTTP API, the
FFI surface and its C header, the configuration keys and their environment
overrides, and the database the migrations build. From 1.0 on:

- **Major** — a change a client or an operator has to act on: an HTTP endpoint
  or an FFI call removed or changed in shape, a configuration key renamed or
  removed, a database that has to be rebuilt rather than migrated.
- **Minor** — something added that existing clients and configurations keep
  working alongside: a new endpoint or FFI call, a new optional configuration
  key, a migration that only adds.
- **Patch** — a fix that changes no part of that contract.

The project is not at 1.0 yet. SemVer allows anything to change in a 0.x
release, and this repository uses that latitude in one specific way: a minor
bump is what a client reads as "not the core I was built against".
`alexandria-ui` accepts a single minor line and refuses anything else at
startup, by name and version, so the minor version is bumped in the same pull
request as any change a client built against the previous version could not
work with — a schema addition, or a call added or changed in shape. Everything
else takes a patch bump.

The workspace version lives in `[workspace.package]` in the root `Cargo.toml`
and is shared by all three crates; it is edited by hand, with a note on the
comment above it saying what the bump is for. It is also what the core reports
as its own version over FFI and what the release is tagged with.

## Releasing

No release has been tagged yet, and the repository has no release workflow: a
release is a tag, and the desktop front-end's packages are what carry the core
to an owner. To release `x.y.z`:

1. On `develop`, through a normal `feature/` or `fix/` pull request — a release
   branch cannot carry commits of its own — make the version in `Cargo.toml`
   `x.y.z` if it is not already, rename `## [Unreleased]` in
   [CHANGELOG.md](./CHANGELOG.md) to `## [x.y.z] - <yyyy-mm-dd>` above a fresh,
   empty `## [Unreleased]`, and update the compare links at the bottom.
2. Cut the release branch from that `develop` and open a pull request into
   `main`:

   ```bash
   git switch develop && git pull
   git switch -c release/x.y.z
   git push -u origin release/x.y.z
   ```

3. Merge it with a merge commit once the checks pass, then tag the merge commit
   on `main` with an annotated tag and push it:

   ```bash
   git switch main && git pull
   git tag -a vx.y.z -m "Alexandria API x.y.z"
   git push origin vx.y.z
   ```

The release branch is deleted after the merge.
