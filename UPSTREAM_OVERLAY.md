# Upstream overlay

This file lists **edits inside files that also exist in**
[`xai-org/grok-build`](https://github.com/xai-org/grok-build)
(plus a few files this fork **adds inside those crates**).

It is the checklist for the next `git merge upstream/main` into `custom`.
Fork-only crates (`crates/codegen/gtm-hub`, `scripts/install-gtm.sh`) are
**not** overlays — they cannot conflict with upstream paths.

Search token in source: **`GTM overlay:`**

```sh
rg -n "GTM overlay:"
```

After a merge, every overlay id below should still `rg`. If a hit vanished,
upstream rewrote the site — restore from the recipe, do not take theirs blindly.

Last official sync SHA: see root [`SOURCE_REV`](SOURCE_REV).

---

## How to re-apply

1. `git fetch upstream` then `git merge upstream/main` on `custom` (do not rebase).
2. For each conflicted overlay file, keep the GTM behavior in the table.
3. `rg -n "GTM overlay:"` and tick the ids.
4. `cargo check -p xai-grok-pager-bin --bin gtm` and the overlay tests named below.
5. Bump `SOURCE_REV` only when the merge is the official sync.

---

## Overlay ids

### `serve-auth` — agent `serve` secret (security)

Upstream printed `ws://…/ws?server-key=<secret>` and accepted that query.
Auto-generated secrets were 12 hex chars (~48 bit). Header compare used `==`.

Keep:

| File | What to keep |
|------|----------------|
| `crates/codegen/xai-grok-pager-bin/src/main.rs` | `print_serve_startup_info`: URL has **no** secret; print token only if stderr is a TTY. `AgentCmd::Serve` uses `get_secret()` `Result` (exit 1 if short). |
| `crates/codegen/xai-grok-pager/src/app/cli.rs` | `MIN_AGENT_SECRET_BYTES = 32`. Auto secret = 32-byte CSPRNG hex. Reject shorter `--secret` / `GROK_AGENT_SECRET`. Tests `serve_*`. |
| `crates/codegen/xai-grok-shell/src/agent/server.rs` | `validate_auth`: **Bearer only** (no `?server-key=`). `secrets_equal`: SHA-256 + `subtle::ConstantTimeEq`. |
| `crates/codegen/xai-grok-shell/src/agent/server_tests.rs` | Bearer accept/reject + query-not-auth tests. |
| `crates/codegen/xai-grok-shell/Cargo.toml` | `subtle = { workspace = true }` |
| `Cargo.toml` | workspace `subtle = "2"` |

If upstream later ships a real serve-auth fix, drop this overlay and delete the markers.

### `gtm-bin` — second cargo binary

| File | What to keep |
|------|----------------|
| `crates/codegen/xai-grok-pager-bin/Cargo.toml` | `[[bin]] name = "gtm"` sharing `src/main.rs`. Must not install over `~/.grok/bin/grok`. |
| `crates/codegen/xai-grok-pager-bin/src/main.rs` | `is_gtm_cli()` (`argv0 == "gtm"`). |

### `hub-cli` — `gtm hub` / `gtm remote` intercept

Hub **implementation** lives in fork-owned `crates/codegen/gtm-hub`. Overlay is only the **hook in the upstream binary**:

| File | What to keep |
|------|----------------|
| `crates/codegen/xai-grok-pager-bin/src/main.rs` | Before `PagerArgs::parse_cli()`, if argv1 is `hub` or `remote`, `exit(gtm_hub::main_from_env())`. Do **not** add `Command::Hub` to pager clap. |
| `crates/codegen/xai-grok-pager-bin/Cargo.toml` | `gtm-hub = { path = "../gtm-hub" }` |
| `Cargo.toml` | workspace member `crates/codegen/gtm-hub` |

### `hub-tui` — TUI hosts a live session on the hub

| File | What to keep |
|------|----------------|
| `crates/codegen/xai-grok-pager/src/app/gtm_hub.rs` | **Added file** (not in upstream). Restore from `custom` if merge deletes it. |
| `crates/codegen/xai-grok-pager/src/app/mod.rs` | `pub mod gtm_hub`, `GtmHubBridge` on `app::run`. |
| `crates/codegen/xai-grok-pager/src/app/event_loop.rs` | Hub inbound/outbound wiring. |
| `crates/codegen/xai-grok-pager/src/app/dispatch/router.rs` (and prompt/queue/task_result as needed) | Host/unhost + fan-out of `session/update`. |
| `crates/codegen/xai-grok-pager-bin/src/main.rs` | `gtm_hub_tui_pump`, `attach_tui` only when `is_gtm_cli()`. |

### `turn-effort` — `/low` `/medium` `/high` `/xhigh`

Turn-scoped effort on the **current** model. Session `/effort` stays upstream.

| File | What to keep |
|------|----------------|
| `crates/codegen/xai-grok-pager/src/slash/commands/turn_effort.rs` | **Added file.** Commands + `_meta.reasoningEffort`. |
| `crates/codegen/xai-grok-pager/src/slash/commands/mod.rs` | Register the four commands. |
| `crates/codegen/xai-grok-pager/docs/user-guide/04-slash-commands.md` | User-facing docs. |
| `crates/codegen/xai-grok-pager/src/app/cli.rs` | `--effort` on agent args if still present. |
| `crates/codegen/xai-grok-pager-bin/src/main.rs` | `reasoning_effort_override` / `reasoning_effort` plumbing. |
| `crates/codegen/xai-grok-shell/src/session/commands.rs` | `SessionCommand::Prompt.reasoning_effort`. |
| `crates/codegen/xai-grok-shell/src/session/acp_session.rs` | Prompt `_meta` → turn input. |
| `crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs` | `process_conversation_turn(_with_recovery)` applies `turn_reasoning_effort` onto the sampling request. |
| `crates/codegen/xai-grok-shell/src/session/acp_session_impl/{goal,interjection,parent_message,prompt_queue,run_loop,turn_task,notification_drain}.rs` | Call sites that construct / forward `TurnInputRequest` — keep the field. |
| `crates/codegen/xai-grok-shell/src/agent/mvp_agent/acp_agent.rs` | ACP prompt path. |
| `crates/codegen/xai-grok-shell/src/agent/subagent/{spawn,attempt_runner}.rs` | Subagent spawn/attempt must pass `None` or the parent field, not drop it. |
| `crates/codegen/xai-grok-shell/src/session/slash_commands.rs` | Slash dispatch. |
| `crates/codegen/xai-grok-shell/src/tools/notification_bridge.rs` | If it clones turn input. |
| matching `*_tests.rs` under `acp_session_tests/` | Keep extra `reasoning_effort: None` after `traceparent` if upstream added that neighbor field. |

Merge tip: if upstream adds a new `TurnInputRequest { … }` struct literal, it will not include `reasoning_effort`. Add `reasoning_effort: None` (or the forwarded value) next to `traceparent`.

### `branding` — user-visible “Grok To Mars”

Do **not** rename wire ids, config paths (`~/.grok`), or telemetry fields.

| File | What to keep |
|------|----------------|
| `crates/codegen/xai-grok-version/src/lib.rs` | `DISPLAY_NAME = "Grok To Mars"` |
| `crates/codegen/xai-grok-pager/src/views/welcome/{mod.rs,hero_box.rs,consent_tests.rs}` | Welcome chrome. |
| `crates/codegen/xai-grok-pager/src/views/{tutorial.rs,question_view.rs}` | Tutorial + feedback copy. |
| `crates/codegen/xai-grok-pager/src/app/mod.rs` | Window/about title. |
| `crates/codegen/xai-grok-pager/src/app/cli.rs` | clap `about`. |
| `crates/codegen/xai-grok-pager-minimal/src/{auth.rs,welcome.rs}` | Minimal-mode strings. |
| `crates/codegen/xai-grok-pager/src/slash/commands/{tutorial.rs,imagine.rs,imagine_video.rs,loop_cmd.rs}` | Copy that still says Grok Build. |
| `crates/codegen/xai-grok-shell/src/auth/oidc/login.rs` | User-facing login chrome only. |
| `README.md`, `CONTRIBUTING.md` | Fork disclaimer. |

### `lru-iter-mut` — GHSA-rhfx-m35p-ff5j

Not an upstream grok-build source file. Workspace `Cargo.toml` **is** regenerated/merged from upstream, so the `[patch.crates-io]` line is an overlay.

| File | What to keep |
|------|----------------|
| `Cargo.toml` | `lru = { path = "third_party/lru" }` under `[patch.crates-io]` |
| `third_party/lru/` | Vendored 0.12.5 + IterMut `&K` (not `&mut K`). |
| `.trivyignore` | `GHSA-rhfx-m35p-ff5j` (version string stays 0.12.5). |

If upstream bumps `aws-sdk-s3` / `ratatui` off `lru ^0.12`, delete the patch and the vendor dir.

### `terminal-theme` — transparent `/theme` on by default

Upstream 1.0.24 ships Terminal (`transparent` / `native`) behind a gradual remote rollout (`default_enabled: false`). Official `grok` hides it until `terminal_theme_enabled` or `GROK_TERMINAL_THEME=1`.

Keep GTM on without that wait:

| File | What to keep |
|------|----------------|
| `crates/codegen/xai-grok-config-types/src/registry.rs` | `Feature::TerminalTheme` `default_enabled: true`. |
| `crates/codegen/xai-grok-pager/src/app/mod.rs` | `resolve_terminal_theme_enabled` ignores remote `false`. Pin / env / config still win. |
| `crates/codegen/xai-grok-pager/docs/user-guide/06-theming.md` | Note that GTM enables it by default. |

---

## Not overlays (fork-owned)

These paths are **absent** from `xai-org/grok-build`. Merge will not touch them unless we add the same path upstream later.

- `crates/codegen/gtm-hub/**` (Unix hub, LAN mTLS, enroll). Enroll default `~/.gtm/gtm-hub.enroll` (0600) and CA `BasicConstraints::Constrained(0)` live **here**, not in grok-build.
- `scripts/install-gtm.sh`
- `SOURCE_REV`, this file

---

## Marker convention

In Rust/TOML/markdown that sits on an upstream path:

```text
GTM overlay: <id> — <one-line keep rule>
```

`<id>` must match a heading above (`serve-auth`, `gtm-bin`, `hub-cli`, `hub-tui`, `turn-effort`, `branding`, `lru-iter-mut`, `terminal-theme`).
