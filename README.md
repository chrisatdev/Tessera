# Tessera

A minimal X11 tiling window manager written in Rust — a greenfield,
single-binary master-stack WM. This change delivers the complete core
skeleton: the pure window-manager core, the x11rb display layer, and the
binary that wires them together, plus a gated end-to-end test against a real
X server.

## Status

Implemented in this change (all unit tests green, headless):

- **Core skeleton + event bus** — typed event bus with non-blocking fan-out
  and a WmState watch, dynamic workspaces (auto-create, clamp ≥ 1, MRU
  switching), master-stack layout, 4-state window lifecycle (unmanage races
  resolved), config with TOML reload (SIGHUP keeps the old config on error).
- **X11 display layer** — x11rb `DisplayServer` implementation: connect with
  abort-on-failure, `WM_S0` claim, SubstructureRedirect event loop, border
  frame reparenting, EWMH desktop-property writes, keycode→keysym translation
  and 15 default keybindings grabbed on the root.
- **Status bar** — X11-drawn workspace tags on a configurable screen edge
  (default top), focused tag highlighted; redrawn once per recompute from the
  WmState watch. Tags-only for now (no clock or tray), thickness per-edge
  configurable.
- **Binary** — connects, claims, and runs the loop; the tiling area is the
  real screen geometry (queried from the root window at startup) minus the
  bar thickness along the configured edge.
- **Theming** — a pure `Theme` in `tessera-core` (embedded ayu_dark default)
  with lenient `theme.toml` parsing; frame borders are theme-driven (active
  vs inactive) and a custom theme is resolved from `config.toml` at startup.
- **Gated E2E** — a real Xvfb server drives the binary as a subprocess with a
  second x11rb test client and xdotool: claim → MapRequest → reparent + tile
  → key-driven focus/workspace-switch handling, plus themed border pixels on
  the real server (default and custom theme files).

Test counts: 177 unit tests (15 binary + 97 core + 65 x11) plus 4 ignored
integration tests. `cargo test --workspace`, `cargo clippy
--workspace --all-targets`, and `cargo fmt --check` are clean.

## Running

```sh
cargo test            # headless unit tests, no X server needed
cargo run             # requires an X display; claims WM_S0 and tiles
```

### Gated end-to-end test (Xvfb)

Prerequisites (not installed by this repository): `Xvfb` and `xdotool`.

```sh
xvfb-run -a -s "-screen 0 1280x1024x24" cargo test --test integration -- --ignored --test-threads=1
```

The tests are `#[ignore]` so plain `cargo test` stays green headless; the
pinned screen size (1280x1024x24, deliberately not 1920x1080) makes the
geometry assertions prove the real-screen wiring. Install `xvfb` and
`xdotool` with your distribution's package manager first. `--test-threads=1`
is required because each test spawns its own WM and `WM_S0` is an exclusive
root selection: two WMs on the same display would conflict, and the second
would abort its claim before its assertions start.

## Installation

Build the release binary, then install it together with the display-manager
session entry:

```sh
cargo build --release
make install                  # PREFIX defaults to /usr
```

- `make install` (see the `Makefile`) copies the `tessera` binary to
  `$(PREFIX)/bin` and the XDG session entry to
  `$(PREFIX)/share/xsessions/tessera.desktop`.
- Install to a different root with `PREFIX` (may need `sudo` for `/usr`):

  ```sh
  make install PREFIX=/usr/local     # or PREFIX=$HOME/.local, no root needed
  ```

- Arch Linux packages: a PKGBUILD can use the staged `DESTDIR` form so the
  `.desktop` file lands inside the package rather than on the host:

  ```sh
  make install DESTDIR="$pkgdir" PREFIX=/usr
  ```

- Once installed, the session selector of **GDM, SDDM, and LightDM** lists
  **Tessera** as a login entry (they auto-discover `share/xsessions/`). The
  returned session starts the `tessera` binary directly — no desktop
  environment or systemd user service is required. A systemd service is
  optional and not needed for the entry to work.

## Architecture

Three crates plus the binary:

```
src/                    tessera (binary) — CLI, wiring, status-bar owner
crates/tessera-core/    pure, zero X deps — event bus, workspaces, layout,
                        window lifecycle, config, DisplayServer trait
crates/tessera-x11/     x11rb implementation of the display seam
```

The core is X-free by design (D1): it drives a `DisplayServer` trait, so the
loop is testable headless on a scripted `MockDisplay`. `tessera-x11`
translates X events into typed core events, claims `WM_S0`, reparents
clients into 2px border frames, and applies placements. The single-threaded
loop publishes every event on the bus; the WmState watch feeds the status bar
with the latest complete snapshot, and the binary calls `bar.draw()` once per
recompute from that snapshot (never on idle polling).

## Configuration

Defaults (a TOML file is optional; `--config <path>` overrides):

| Setting | Default |
|---|---|
| `general.border_width` | 2 |
| `general.gaps` | 0 |
| `general.terminal` | `alacritty` |
| `general.theme` | (none — embedded ayu_dark) |
| Layout | master-stack, master ratio 0.5 |

### Status bar

The `[bar]` table configures the status bar. It is optional — with no table,
all defaults apply (top edge, visible, default colours). Every field is
optional:

| Field | Default | Notes |
|---|---|---|
| `position` | `"top"` | `Top`/`Bottom`/`Left`/`Right` — the screen edge the bar is drawn on |
| `thickness` | per-edge default | **No default value:** when unset, thickness is the per-edge default — 22px along the top/bottom, 6px along the left/right. An explicit value applies to every edge and must be within `1..=200`. |
| `bg_color` | `"#222222"` | Bar background (`#RRGGBB` only) |
| `fg_color` | `"#eeeeee"` | Focused-tag foreground (`#RRGGBB` only) |
| `visible` | `true` | `false` hides the bar; the full screen stays available for tiling |

Parsing is **strict** (like `general` and `keybindings`): an unknown `[bar]`
key (e.g. `flavor = "cherry"`) or an invalid value (a position that is not a
screen edge, thickness `0` or `> 200`, a colour that is not `#RRGGBB`)
rejects the whole file at startup — the running config is kept on SIGHUP.

Example:

```toml
[bar]
position = "bottom"
thickness = 28
bg_color = "#222222"
fg_color = "#eeeeee"
visible = true
```

The bar shows the workspace tags of the current `WmState`, highlighting the
focused tag, and is redrawn exactly once per layout recompute.

### Theming

Frame borders are painted from a `Theme` palette owned by `tessera-core`:

- **Default**: with no `theme` in the config, the embedded **ayu_dark**
  palette is used and no file is read. Focused frames use the `accent`
  colour (`#FF8F40`), unfocused frames the `comment` colour (`#626A73`).
- **Custom theme**: point at a `theme.toml` file:

  ```toml
  [general]
  theme = "themes/ayu_dark.toml"
  ```

  See [`themes/ayu_dark.toml`](themes/ayu_dark.toml) for the full reference
  file. Every key is optional — a missing key falls back to the embedded
  ayu_dark value for that field (lenient per-field fallback), while unknown
  keys are rejected. The optional `active_border` / `inactive_border` keys
  override the derived defaults.
- **Broken file**: a `theme` path that is missing or cannot be parsed never
  aborts startup — the WM prints a warning and falls back to the embedded
  ayu_dark palette. (This differs from config files, which abort at boot:
  there is no "previous theme" to keep, and the palette is decorative.)

Keybindings (all Super-based, configurable):

| Keys | Action |
|---|---|
| Super+Enter | spawn terminal |
| Super+J / Super+K | focus next / previous |
| Super+Q | close focused |
| Super+1..9, Super+0 | switch to workspace 1..10 |
| Super+Space | toggle layout |

SIGHUP reloads the config; a bad file is rejected and the running config is
kept.

## Known v1 limits

- The bar is tags-only — it has no clock, tray, or layout indicator yet. The
  font is the X11 core font `fixed`; where that font is unavailable, tags are
  drawn as filled rectangles without text.
- The bar is drawn on the primary RandR output only; on a setup with no
  primary output, the first connected output is used.
- The theme is resolved once at startup; SIGHUP reloads the config but not a
  changed `theme` path (restart the WM to pick up a new theme file).
- A workspace is auto-created on demand only when none exist; there is no
  command yet to open or switch to an empty workspace.
- EWMH desktop-property writes are wired as a display-layer delegate but not
  yet driven by the loop.
- Master-stack is the only layout.

## What's next

- Bar content: clock, tray, and a layout indicator.
- The remaining four layouts (tall, spiral, grid, ...).
- Workspace UX: opening/renaming workspaces, switching to empty ones.
