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
- **Binary** — connects, claims, and runs the loop; the tiling area is the
  real screen geometry (queried from the root window at startup). The bar is
  a placeholder that renders the current WmState snapshot once.
- **Theming** — a pure `Theme` in `tessera-core` (embedded ayu_dark default)
  with lenient `theme.toml` parsing; frame borders are theme-driven (active
  vs inactive) and a custom theme is resolved from `config.toml` at startup.
- **Gated E2E** — a real Xvfb server drives the binary as a subprocess with a
  second x11rb test client and xdotool: claim → MapRequest → reparent + tile
  → key-driven focus/workspace-switch handling, plus themed border pixels on
  the real server (default and custom theme files).

Test counts: 145 unit tests (14 binary + 86 core + 45 x11) plus 4 ignored
integration tests. `cargo clippy --workspace --all-targets` and
`cargo fmt --check` are clean.

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

## Architecture

Three crates plus the binary:

```
src/                    tessera (binary) — CLI, wiring, bar placeholder
crates/tessera-core/    pure, zero X deps — event bus, workspaces, layout,
                        window lifecycle, config, DisplayServer trait
crates/tessera-x11/     x11rb implementation of the display seam
```

The core is X-free by design (D1): it drives a `DisplayServer` trait, so the
loop is testable headless on a scripted `MockDisplay`. `tessera-x11`
translates X events into typed core events, claims `WM_S0`, reparents
clients into 2px border frames, and applies placements. The single-threaded
loop publishes every event on the bus; the WmState watch feeds consumers
such as the bar with the latest complete snapshot.

## Configuration

Defaults (a TOML file is optional; `--config <path>` overrides):

| Setting | Default |
|---|---|
| `general.border_width` | 2 |
| `general.gaps` | 0 |
| `general.terminal` | `alacritty` |
| `general.theme` | (none — embedded ayu_dark) |
| Layout | master-stack, master ratio 0.5 |

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

- The bar is a placeholder (single snapshot render), not a live per-frame bar.
- The theme is resolved once at startup; SIGHUP reloads the config but not a
  changed `theme` path (restart the WM to pick up a new theme file).
- A workspace is auto-created on demand only when none exist; there is no
  command yet to open or switch to an empty workspace.
- EWMH desktop-property writes are wired as a display-layer delegate but not
  yet driven by the loop.
- Master-stack is the only layout.

## What's next

- Real bar with live per-iteration rendering.
- Tray support and a plugin interface.
- The remaining four layouts (tall, spiral, grid, ...).
- Workspace UX: opening/renaming workspaces, switching to empty ones.
