# tacoterm

A GPU-accelerated terminal emulator for Windows, written in Rust.

The real console lives in a Windows pseudo-console (ConPTY); tacoterm is a fast
window onto it. `cmd.exe`, PowerShell and WSL all just work, because from their
side nothing is unusual — they talk to a console, as always.

## Features

Tabs and split panes onto PowerShell (the default), `cmd.exe` or WSL, drawn on the GPU:

- **Tabs**: Ctrl+Shift+T opens one (in the current folder), Ctrl+Tab /
  Ctrl+Shift+Tab switch, click the tab strip, middle-click or × closes
- **Split panes**: Alt+Shift+= splits right, Alt+Shift+- splits down,
  Alt+arrows move between panes, Alt+Shift+arrows resize, Ctrl+Shift+W closes
- **Transparency**: the whole window, title bar included, with `opacity`
  plus Windows 11 acrylic / mica backgrounds. tacoterm draws its own title
  bar into the tab strip (drag it, double-click to maximize, snap works);
  `title_bar = "system"` brings back the Windows one
- **Settings page**: the ⚙ in the title bar or Ctrl+, opens every setting
  beside a live preview
- **Find in scrollback**: Ctrl+Shift+F; Enter / Shift+Enter step through hits
- **Links**: Ctrl+click a URL to open it (it underlines while Ctrl is held)
- **Mouse**: drag, double-click a word, triple-click a line, Shift+click
  extends; apps like vim, htop and tmux get mouse events (hold Shift to
  select instead)
- **Zoom** with Ctrl+= / Ctrl+- / Ctrl+0 or Ctrl+wheel; **full screen** with F11
- **Summon hotkey**: Win+` shows or hides tacoterm from anywhere;
  `quake_mode = true` drops it down from the top of the screen
- Blinking cursor shapes (and apps can change them), visual or audible bell,
  apps can copy to the clipboard (OSC 52)
- Text rewraps when the window is resized
- Box-drawing and block characters drawn to tile without gaps
- VT parsing for what ConPTY and full-screen apps emit: cursor movement,
  erase, insert/delete, scroll regions, alternate screen, SGR (16 / 256 /
  truecolor), titles, status reports, bracketed paste, focus events
- Wide characters (CJK, emoji) and font fallback via cosmic-text
- Built-in color schemes and a colored prompt; `config.toml` applies live
- Per-monitor DPI scaling

## Build (Windows)

```
cargo run --release
```

The window and ConPTY are Windows-only. The terminal core (grid, VT
parser, renderer, config, key mapping) is a portable library, so
`cargo test` runs anywhere; the render test draws a frame offscreen and
checks its pixels (it skips when no GPU adapter, not even a software one,
is available). `TACOTERM_SNAPSHOT=frame.png cargo test --test render` saves
that frame (`TACOTERM_SNAPSHOT_TABS` saves the tabs-and-splits one,
`TACOTERM_SNAPSHOT_SETTINGS` the settings page).

## Settings

Click the **⚙** in the title bar (or press Ctrl+, or right-click →
Settings…) for the settings page: every option in a list, with a preview
terminal beside it. Changes show on the whole window as you make them;
**Save** (Ctrl+S) keeps them, **Cancel** (Esc) puts things back. Use the
arrow keys or the mouse: ‹ › cycle a choice, drag a slider, click a toggle.

**Right-click** anywhere in tacoterm for tabs, splits, find, copy/paste,
the color scheme, opacity, background effect, the shell (PowerShell,
Command Prompt, WSL), full screen, the settings page and **Edit
config.toml**, which opens `%APPDATA%\tacoterm\config.toml` in Notepad.
Save that file and colors, fonts, cursor and keybindings apply right away;
shell and prompt apply to the next shell. Every key is optional, and a
mistake shows a warning and falls back to the default.

```toml
# tokyo-night, catppuccin-mocha, dracula, one-half-dark, gruvbox-dark,
# nord, solarized-dark, one-half-light, campbell
color_scheme = "tokyo-night"     # Ctrl+Shift+N previews the next one
cursor_style = "bar"             # block, bar or underline
font_family = "Cascadia Mono"    # falls back to Consolas, then others
font_size = 14.0
padding = 10
opacity = 0.85                   # 1.0 is solid
backdrop = "acrylic"             # none, acrylic, mica or tabbed (Windows 11)

shell = "auto"                   # PowerShell; or "cmd.exe", "wsl.exe ~", …
prompt = "arrow"                 # path + ❯; "classic" = the shell's own
right_click = "menu"             # or "paste", like the old console
title_bar = "custom"             # see-through; "system" = the Windows one

[theme]                          # override single colors of the scheme
background = "#101010"

[keybindings]
copy = "ctrl+shift+c"
paste = "ctrl+shift+v"
new_tab = "ctrl+shift+t"
split_right = "alt+shift+="
find = "ctrl+shift+f"
```

The file tacoterm writes on first run lists every key and binding.

## Layout

| File | What it holds |
| --- | --- |
| `src/main.rs` | Entry point (Windows binary) |
| `src/app.rs` | winit event loop, tabs and panes, mouse/keyboard, find |
| `src/app/page.rs` | The settings page: input and live preview |
| `src/menu.rs` | The right-click menu |
| `src/win.rs` | Win32 bits: summon hotkey, backdrop, links, beep |
| `src/pty.rs` | ConPTY wrapper: spawn, resize, write, exit watch |
| `src/lib.rs` | The portable terminal core |
| `src/vt.rs` | `vte` parser → grid |
| `src/grid.rs` | Screen, scrollback, reflow, selection, search, links |
| `src/input.rs` | Key events → VT bytes; keybinding matching |
| `src/mouse.rs` | Mouse reporting encodings |
| `src/layout.rs` | Split-pane tree |
| `src/boxdraw.rs` | Pixel-exact box-drawing and block glyphs |
| `src/renderer.rs` | wgpu renderer: panes, tab strip, find box, glyph atlas |
| `src/config.rs` | TOML settings, written on first run |
| `src/settings.rs` | What the settings page lists and how each value steps |
| `src/schemes.rs` | Built-in color schemes |

## Roadmap

- [x] ConPTY, VT parsing, grid, scrollback, alternate screen
- [x] GPU renderer, box drawing, transparency
- [x] Tabs, splits, find, links, mouse reporting, reflow, quake mode
- [x] See-through title bar, settings page with live preview
- [ ] Per-row damage tracking, OSC 8 hyperlinks, dragging pane dividers
- [ ] A shell of our own

## License

MIT, (c) 2026 tacobellerontop-sudo.
