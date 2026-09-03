# Grok To Mars (`gtm`)

> [!IMPORTANT]
> This is an **unofficial personal fork** of SpaceXAI's Grok Build CLI.
> It is **not** an official SpaceXAI or xAI product, and it is **not** the
> [xai-org/grok-build](https://github.com/xai-org/grok-build) repository.
>
> Official product: [x.ai/cli](https://x.ai/cli) ·
> Official source: [github.com/xai-org/grok-build](https://github.com/xai-org/grok-build) ·
> Official docs: [docs.x.ai/build](https://docs.x.ai/build/overview)

Grok To Mars is a source-built coding-agent TUI based on the public
[Grok Build](https://github.com/xai-org/grok-build) tree. It understands a
codebase, edits files, runs shell commands, and can run interactively,
headlessly, or over ACP — the same harness as upstream, with a few local
changes.

This fork installs as **`gtm`**. It is meant to sit **next to** an official
`grok` install, not replace it.

[Install `gtm`](#install-gtm) ·
[Official Grok Build](#official-grok-build) ·
[What this fork changes](#what-this-fork-changes) ·
[Remote hub](#remote-hub) ·
[Branches](#branches) ·
[Building from source](#building-from-source) ·
[Documentation](#documentation) ·
[License](#license)

## Official Grok Build

If you want the released Grok Build CLI, **do not install this repository**.
Use SpaceXAI's installer and the upstream tree:

```sh
curl -fsSL https://x.ai/cli/install.sh | bash   # macOS / Linux / Git Bash
irm https://x.ai/cli/install.ps1 | iex          # Windows PowerShell
grok --version
```

Upstream README, changelog, and source:

- [xai-org/grok-build](https://github.com/xai-org/grok-build)
- [x.ai/cli](https://x.ai/cli)
- [changelog](https://x.ai/build/changelog)

| | Official Grok Build | This fork (Grok To Mars) |
| --- | --- | --- |
| Command | `grok` | `gtm` |
| Typical path | `~/.grok/bin/grok` | `~/.local/bin/gtm` |
| Source | [xai-org/grok-build](https://github.com/xai-org/grok-build) | this repository |
| Install | [x.ai/cli](https://x.ai/cli) | [`scripts/install-gtm.sh`](scripts/install-gtm.sh) |
| Updates | `grok update` | rebuild with `scripts/install-gtm.sh` (`gtm update` is disabled on purpose) |

`gtm` still authenticates against the same Grok services as the official CLI.

## What this fork changes

Relative to upstream Grok Build, this tree currently adds:

- Turn-scoped reasoning-effort commands: `/low`, `/medium`, `/high`, `/xhigh`
  (one prompt on the current model; session `/effort` is unchanged)
- A second cargo binary named `gtm`, installed beside official `grok`
- User-facing TUI chrome labeled **Grok To Mars** instead of Grok Build
  (welcome title, help, tutorial, feedback)
- A local **session hub** (`gtm hub` / `gtm remote`) so a `gtm` TUI and a
  remote client (macOS/iOS app) can attach to the **same** live session.
  Official `grok` sessions are **not** shared this way — see [Remote hub](#remote-hub).

Everything else is Grok Build, merged from upstream. The root `SOURCE_REV`
file records the monorepo commit SHA of the last official sync.

Edits that sit **inside upstream files** (pager, shell, workspace `Cargo.toml`, …)
are catalogued in [`UPSTREAM_OVERLAY.md`](UPSTREAM_OVERLAY.md) so a
`git merge upstream/main` can re-apply them. Search token: `GTM overlay:`.

## Remote hub

`gtm` can run a machine-local hub (`~/.gtm/hub.sock`). The interactive
`gtm` TUI connect-or-spawns it. Phone/tablet clients join over LAN TLS
(mTLS, enroll file), not the Unix socket.

```sh
gtm hub status                 # pid, socket, which sessions are live
gtm remote --lan               # bind TLS 1.3 mTLS on :27420 (ALPN gtm-hub)
gtm remote enroll [--ttl 30d]  # write ~/.gtm/gtm-hub.enroll (0600)
gtm remote list | revoke <id> | off | status
```

Disk transcripts still live under `~/.grok/sessions/…` (same layout as
official Grok Build). The hub does **not** create a second session
database. What it *does* decide is **which process owns the live turn**.

### `gtm` session vs official `grok` session

Both binaries can open the same session **id** on disk. That does not mean
they share the live agent.

| | Official `grok` TUI | `gtm` TUI |
| --- | --- | --- |
| Command | `grok` / `grok --resume <id>` | `gtm` / `gtm --resume <id>` |
| Live agent | In-process (or grok leader) | In-process, and **hosts** that session on the hub |
| Remote app | Hub cannot attach to this actor | Hub forwards prompts into this TUI and clones `session/update` back |
| `gtm hub status` | Session does not show as `[TUI host]` | Session shows `[TUI host]` when the TUI has it open |

The remote app always talks to **gtm hub**, never to `~/.grok/leader.sock`
and never to a running `grok` process.

If you resume an official **grok CLI session** from the app while a `grok`
TUI already has it open:

- The hub starts a **private** actor (`gtm agent --no-leader stdio`).
- That actor is a **second** runtime on the same session files.
- TUI input/output and app input/output are **not** cloned. Each side only
  sees what its own actor streams. Reloading history from disk may show
  past turns; the in-flight turn is not mirrored.

The same split happens if the app attaches while **no** `gtm` TUI is
hosting: `gtm hub status` lists the session as `[hub actor]`. That is a
phone-only (or app-only) agent, not a clone of a `grok` window.

### What *is* shared (and what is not)

**Shared**

- Session files on disk (`summary.json`, `chat_history.jsonl`, …) — any
  client can list and load history for a session id.
- Live input **and** output between a **`gtm` TUI host** and a remote app
  attached to that same id (`[TUI host]`). App prompt → hub → TUI;
  TUI stream → hub → app.

**Not shared**

- A live `grok` TUI and the remote app (separate actors, even on the same
  session id).
- Two `gtm` TUI windows on the same session (last TUI to host wins; it
  does not merge two in-process agents).
- Permissions for dangerous tools from a LAN client — those still need a
  local Mac/`gtm` confirmation.
- Official grok leader, MCP hosts, or `grok agent serve` as the remote
  transport.

### How to share a session with the app

1. Quit any official `grok` TUI that has the session open (optional but
   avoids two writers on the same files).
2. Open it with **this** binary, not `grok`:

   ```sh
   gtm --resume <session-id>
   ```

3. In another terminal:

   ```sh
   gtm hub status
   ```

   You want `[TUI host]` on that id, not `[hub actor]` and not
   `live sessions: none`.
4. Connect the app with a `gtm-hub.enroll` file and **select that same
   session**.

`gtm hub status` is only a probe; it does not host a session. Hosting
starts when the interactive `gtm` TUI has the session open.

After `scripts/install-gtm.sh`, already-running `gtm` processes keep the
old binary in memory. Restart the TUI (and the hub if you were asked to)
so the installed file is what is actually executing.

## Install `gtm`

Requirements are the same as [building from source](#building-from-source).

```sh
git clone https://github.com/gegegi/grok-to-mars-cli.git
cd grok-to-mars-cli
git checkout custom
scripts/install-gtm.sh
gtm --version
```

The script builds a release `gtm` and copies it to `~/.local/bin/gtm`
(override with `GTM_BIN_DIR`). It refuses to write into `~/.grok/bin`, which
belongs to the official installer.

On first launch, `gtm` opens a browser to authenticate — see the upstream
[authentication guide](crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md).

## Branches

| Branch | What it is |
| --- | --- |
| `main` | Mirror of official [`xai-org/grok-build`](https://github.com/xai-org/grok-build) `main` (no Grok To Mars changes) |
| `custom` | **Default branch.** This fork's line: `gtm`, branding, and extra commands |

Feature branches are merged into `custom` and then deleted. Official updates
are merged from `upstream/main` into `custom` (not rebased).

## Building from source

Requirements:

- **Rust** — the toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml);
  `rustup` installs it automatically on first build.
- **[DotSlash](https://dotslash-cli.com)** — required so hermetic tools under
  [`bin/`](bin/) (notably [`bin/protoc`](bin/protoc)) can download and run.
  Install it and ensure `dotslash` is on your `PATH` **before** building:

  ```sh
  cargo install dotslash
  # or: prebuilt packages — https://dotslash-cli.com/docs/installation/
  /usr/bin/env dotslash --help   # sanity check
  ```

- **protoc** — proto codegen resolves [`bin/protoc`](bin/protoc) via DotSlash,
  or falls back to a `protoc` on `PATH` / `$PROTOC`.
- macOS and Linux are supported build hosts; Windows builds are best-effort
  and not currently tested from this tree.

```sh
cargo run -p xai-grok-pager-bin --bin gtm     # build + launch this fork
cargo build -p xai-grok-pager-bin --release --bin gtm
scripts/install-gtm.sh                        # install as ~/.local/bin/gtm
cargo check -p xai-grok-pager-bin             # fast validation
```

The cargo artifact names are still `xai-grok-pager` (upstream) and `gtm`
(this fork). Running the `xai-grok-pager` target does not replace
`~/.grok/bin/grok`.

## Documentation

Behavior that is not listed under [What this fork changes](#what-this-fork-changes)
matches official Grok Build. Use the upstream docs, substituting `gtm` for
`grok` where you are running this binary:

- [docs.x.ai/build/overview](https://docs.x.ai/build/overview)
- User guide in this tree:
  [`crates/codegen/xai-grok-pager/docs/user-guide/`](crates/codegen/xai-grok-pager/docs/user-guide/)

Turn-scoped effort commands are documented in
[`04-slash-commands.md`](crates/codegen/xai-grok-pager/docs/user-guide/04-slash-commands.md).

Hub crate notes: [`crates/codegen/gtm-hub/README.md`](crates/codegen/gtm-hub/README.md).
The protocol sketch in the umbrella repo is `docs/hub.md`.
After merging official `grok-build`, re-apply overlays from
[`UPSTREAM_OVERLAY.md`](UPSTREAM_OVERLAY.md).

## Repository layout

| Path | Contents |
|------|----------|
| `crates/codegen/gtm-hub` | Local session hub (`gtm hub` / `gtm remote`); not in upstream pager |
| `crates/codegen/xai-grok-pager-bin` | Composition-root package; builds `xai-grok-pager` and `gtm` |
| `crates/codegen/xai-grok-pager` | The TUI: scrollback, prompt, modals, rendering |
| `crates/codegen/xai-grok-shell` | Agent runtime + leader/stdio/headless entry points |
| `crates/codegen/xai-grok-tools` | Tool implementations (terminal, file edit, search, ...) |
| `crates/codegen/xai-grok-workspace` | Host filesystem, VCS, execution, checkpoints |
| `crates/codegen/...` | The rest of the CLI crate closure (config, MCP, markdown, sandbox, ...) |
| `crates/common/`, `crates/build/`, `prod/mc/` | Small shared leaf crates pulled in by the closure |
| `third_party/` | Vendored upstream source (Mermaid diagram stack) |
| `scripts/install-gtm.sh` | Install this fork as `gtm` without touching official `grok` |
| `UPSTREAM_OVERLAY.md` | Fork edits inside files that also exist in `xai-org/grok-build` |

> [!IMPORTANT]
> The root `Cargo.toml` (workspace members, dependency versions, lints,
> profiles) is **generated** — treat it as read-only. Prefer editing per-crate
> `Cargo.toml` files.

## Development

```sh
cargo check -p <crate>        # always target specific crates; full-workspace builds are slow
cargo test -p xai-grok-config # per-crate tests
cargo clippy -p <crate>       # lint config: clippy.toml at the repo root
cargo fmt --all               # rustfmt.toml at the repo root
```

## Contributing

This repository is an unofficial fork. SpaceXAI's upstream tree **does not**
accept external pull requests — see
[`xai-org/grok-build` CONTRIBUTING](https://github.com/xai-org/grok-build/blob/main/CONTRIBUTING.md).

This fork is maintained for personal use. There is no support SLA, and
patches are not solicited.

Security issues in Grok Build itself should be reported through the upstream
[security policy](https://github.com/xai-org/grok-build/blob/main/SECURITY.md)
(HackerOne). Do not open a public GitHub issue for vulnerabilities.

## License

First-party code in the upstream Grok Build tree is licensed under the
**Apache License, Version 2.0** — see [`LICENSE`](LICENSE). This fork keeps
that license. Copyright for the original work remains with SpaceXAI.

Third-party and vendored code remains under its original licenses. See:

- [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES) — crates.io / git dependencies,
  bundled UI themes, and **in-tree source ports** (including openai/codex and
  sst/opencode tool implementations)
- [`crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md`](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md)
  — crate-local notice for the codex and opencode ports (license texts +
  Apache §4(b) change notice)
- [`third_party/NOTICE`](third_party/NOTICE) — vendored Mermaid-stack index
