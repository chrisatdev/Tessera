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

Test counts: 208 unit tests (24 binary + 109 core + 75 x11) plus 16 ignored
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

The config file is optional. When no `--config <path>` is given, Tessera
auto-detects it: `$XDG_CONFIG_HOME/Tessera/tessera.toml`, or
`~/.config/Tessera/tessera.toml` when `XDG_CONFIG_HOME` is unset, empty or
relative. On first run the file does not exist, so Tessera creates it from a
commented template (`tessera: created default config at <path>` in the log)
and loads it — every value in the template is a default, so a fresh install
behaves exactly like an unconfigured one.

Precedence and failure handling differ between the two paths:

| Path | Missing file | Malformed file |
|---|---|---|
| explicit `--config <path>` | aborts startup (nothing is ever auto-created) | aborts startup (strict, unchanged) |
| auto-detected | first run: template created, logged, loaded | warns `cannot parse … using defaults` and keeps going |

With neither `$HOME` nor `$XDG_CONFIG_HOME` set, the WM warns and uses the
defaults. Edit the created file and restart (or SIGHUP) to apply changes.

Defaults (all values in the first-run template):

| Setting | Default |
|---|---|
| `general.border_width` | 2 |
| `general.gaps` | 3 (6px between windows, 3px at the screen edge) |
| `general.terminal` | `alacritty` |
| `general.launcher` | `rofi -show drun` |
| `general.theme` | (none — embedded ayu_dark) |
| Layout | master-stack, master ratio 0.5 |

### Launcher

The `[general] launcher` configures the program that **Ctrl+Space** spawns
(the default is `rofi -show drun`). It is an argv list — each entry is
passed to the program verbatim, with **no shell** in between:

```toml
[general]
launcher = ["dmenu_run"]
```

An explicitly empty list (`launcher = []`) is rejected at parse time: a
launcher that silently does nothing is never accepted. A launcher that is
missing from `PATH` is logged and the WM keeps running.

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
| `font` | `"/usr/share/fonts/TTF/HackNerdFontMono-Regular.ttf"` | **Absolute path** to a TTF/OTF file — not a family name. Glyphs are rasterised in the client and blitted, so a Nerd Font works; an unreadable or unparseable file warns once and falls back to the `fixed` X core font. |
| `font_size` | `12.0` | Glyph size in pixels per em for `font` |

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
font = "/usr/share/fonts/TTF/HackNerdFontMono-Regular.ttf"
font_size = 12.0
```

The bar shows the workspace tags of the current `WmState`, highlighting the
focused tag, and is redrawn exactly once per layout recompute.

Tag glyphs are rasterised **client-side** and uploaded with `PutImage`: the X
core font protocol only reaches bitmap fonts in the server's font path, so it
cannot load a TTF/OTF Nerd Font at all. Because the tag background is a solid
colour the bar itself just painted, each pixel is composited exactly against
that fill — no XRender and no Xft. `font` is a path rather than a family name
because resolving a family requires fontconfig (a C dependency) or shelling
out to `fc-match`; `fc-match 'Hack Nerd Font Mono'` resolves to the default
path above.

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

Keybindings (configurable):

| Keys | Action |
|---|---|
| Super+Enter | spawn terminal |
| Ctrl+Space | spawn launcher (`[general] launcher`) |
| Super+J / Super+K | focus next / previous (MRU ring, wraps) |
| Super+Q | close focused |
| Super+1..9, Super+0 | switch to workspace 1..10 |
| Super+Space | toggle layout |
| Super+H / Super+L | step to the previous / next workspace, by numeric id, wraps |
| Super+Shift+1..9, Super+Shift+0 | send focused window to workspace 1..10, without following it |
| Super+Shift+H/J/K/L | move focus to the window left/down/up/right of the focused one, **does not wrap** |

CapsLock, NumLock and ScrollLock states do not affect bindings: pressing a
key with any lock modifier on still triggers its base binding (the WM
matches the lock-variant grab and strips the lock bits before lookup).

**Directional focus does not wrap, unlike everything else above.** The
`Super+J`/`Super+K` MRU ring and the `Super+H`/`Super+L` workspace ring both
wrap at their ends — a ring always has a well-defined "next" element, even at
the boundary. Directional focus (`Super+Shift+H/J/K/L`) resolves against
real on-screen geometry instead: it moves focus to whichever window's
placement is nearest in that direction, and with no candidate at all it is a
silent no-op. Window space is a plane, not a ring — there is no well-defined
"next window to the right" once nothing is there, so inventing one would
teleport focus across the screen. This is deliberate, not a bug, but it
reads like one the first time: **from a full-height master window,
`Super+Shift+J` and `Super+Shift+K` do nothing**, because with the default
master-stack layout the stack sits to the *right* of the master, not below
it — there is nothing above or below to focus. `Super+Shift+H`/`Super+Shift+L`
(left/right) work as expected from the master. Discoverable beats
surprising, hence this note.

SIGHUP reloads the config; a bad file is rejected and the running config is
kept.

### Diagnosing the Super key

Every default binding is a Super (Mod4) combo, so when Super bindings never
fire, the first step is to prove the guest actually receives the key. At
claim time Tessera logs the keycodes that carry Mod4 and names any binding
whose keysym resolved nowhere in the mapping:

```text
tessera: grabbed 128 lock-variant grabs for 16 bindings; 0 keycodes with NoSymbol keysym
tessera: mod4 keycodes: 133 (Super_L), 134 (Super_R)
```

| Evidence command | What it shows |
|---|---|
| `grep grabbed ~/.xsession-errors` | the claim line: the `grabbed … for 16 bindings` count, the `mod4 keycodes:` diagnosis and any `missing:` entries |
| `xev -event keyboard` | press Super and check for a `KeyPress` event carrying the `Super_L` keysym |
| `xmodmap -pm` | the Mod4 column: exactly which keycodes currently carry Mod4 |
| `Ctrl+Space` | the launcher binding is Control-based — it still fires when the host captures Super |
| `tessera --display :0 2>/tmp/claim.log` | a fresh claim log captured to a file for inspection |

| Symptom | Cause | Action |
|---|---|---|
| claim log says `WARNING: no keycode mapped to Mod4` | no keycode carries Mod4 (SUP-1) | fix the keymap so a key maps to Mod4 |
| claim log ends with `missing: <name> (0x…)` | the binding's keysym resolves to no keycode (KBR-3) | fix the keymap / keysym, or rebind via config |
| `xmodmap -pm` shows the key under the wrong modifier | the key is not in the Mod4 column | remap the key with `xmodmap` or rebind via config |
| `xev` shows no `KeyPress` for Super at all | the host captures the key — the guest never sees it | the only docs-only outcome: see the VM workaround below |

#### Super in VM guests

VirtualBox's Host key is Right Ctrl, so a bare Super usually reaches the
guest; some hosts (VMware, remote-desktop servers) capture Super globally,
and the guest never receives a `KeyPress` — no amount of guest-side remapping
helps, because the key never arrives. The workaround is to rebind the
affected bindings off Super in the config file (created on first run at
`~/.config/Tessera/tessera.toml`, or wherever `--config` points), changing
`mods = 64` (Super) to `mods = 4` (Control), then restart the WM (or SIGHUP
to reload):

```toml
[keybindings.terminal]     # now Ctrl+Enter: open a terminal
mods = 4
key = 65293
```

The template's per-binding comments show the full table; `mods` is the X11
modifier mask (Shift=1, Control=4, Mod1/Alt=8, Mod4/Super=64) and `key` is
the keysym in decimal (Return=65293, space=32, 1..9=49..57, 0=48).

## Known v1 limits

- The bar is tags-only — it has no clock, tray, or layout indicator yet. The
  font is the X11 core font `fixed`; where that font is unavailable, tags are
  drawn as filled rectangles without text.
- The bar is drawn on the primary RandR output only; on a setup with no
  primary output, the first connected output is used.
- The theme is resolved once at startup; SIGHUP reloads the config but not a
  changed `theme` path (restart the WM to pick up a new theme file).
- A workspace is auto-created on demand when none exist, and switching to an
  empty workspace tag (Super+2..0) auto-creates it empty (dynamic workspaces).
- EWMH desktop-property writes are wired as a display-layer delegate but not
  yet driven by the loop.
- Master-stack is the only layout.

## What's next

- Bar content: clock, tray, and a layout indicator.
- The remaining four layouts (tall, spiral, grid, ...).
- Workspace UX: renaming workspaces, and creating workspaces beyond the
  Super+1..0 tags (e.g. numbered/unbounded workspace sets).
