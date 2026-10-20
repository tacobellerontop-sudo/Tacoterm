# tacoterm

A GPU-accelerated terminal emulator for Windows, written in Rust.

The real console lives in a Windows pseudo-console (ConPTY); tacoterm is a fast
window onto it. `cmd.exe`, PowerShell and WSL all just work, because from their
side nothing is unusual — they talk to a console, as always.

## Features

Tabs and split panes onto PowerShell (the default), `cmd.exe` or WSL, drawn on the GPU:

- **Command palette**: Ctrl+Shift+P lists every action (with its key), color
  scheme and shell; type a few letters to filter, Enter runs it
- **Quick select**: Ctrl+Shift+Space labels every URL, path, git hash, IP
  and long number on screen; type a label to copy it (in capitals to paste
  it too)
- **Tabs**: Ctrl+Shift+T opens one (in the current folder), Ctrl+Tab /
  Ctrl+Shift+Tab switch, click the tab strip, middle-click or × closes, drag
  a tab to reorder, double-click to rename it; right-click + to pick the
  shell for the new tab
- **Split panes**: Alt+Shift+= splits right, Alt+Shift+- splits down,
  Alt+arrows move between panes, Alt+Shift+arrows resize (or drag the
  divider), Ctrl+Shift+W closes
- **Shell integration**: Ctrl+Shift+Up / Down jump between prompts (OSC 133);
  programs can show a notification (OSC 9, OSC 777) while tacoterm is in
  the background and drive the taskbar progress bar (OSC 9;4)
- **Transparency**: the whole window, title bar included, with `opacity`
  plus Windows 11 acrylic / mica backgrounds. tacoterm draws its own title
  bar into the tab strip (drag it, double-click to maximize, snap works);
  `title_bar = "system"` brings back the Windows one
- **Settings page**: the ⚙ in the title bar or Ctrl+, opens every setting
  beside a live preview
- **Find in scrollback**: Ctrl+Shift+F; Enter / Shift+Enter step through hits
- **Links**: Ctrl+click a URL or an OSC 8 hyperlink to open it (it underlines
  while Ctrl is held)
- **Text**: emoji sequences (👍🏽, 👨‍👩‍👧, flags) and combining accents in one
  cell; curly, double, dotted and dashed underlines in any color (as
  editors use for errors)
- **Background picture**: `background_image` puts a wallpaper behind the
  text, faded by `background_image_opacity`
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
- **Pictures**: the kitty graphics protocol (PNG, raw RGB/RGBA, zlib,
  chunked, files, placements and deletes), sixel and iTerm2 inline images
  (PNG, JPEG, GIF, BMP, WebP). They scroll with the text. Needs the bundled
  console (see below)
- VT parsing for what ConPTY and full-screen apps emit: cursor movement,
  erase, insert/delete, scroll regions, alternate screen, SGR (16 / 256 /
  truecolor), titles, status reports, bracketed paste, focus events
- Wide characters (CJK, emoji) and font fallback via cosmic-text
- Built-in color schemes and a colored prompt; `config.toml` applies live
- Per-monitor DPI scaling

## Install

Download `tacoterm-<version>-setup.exe` (the **tacoterm-setup** artifact of
the latest Windows CI run) and run it. It installs for just you, no admin
needed (or for everyone, if you pick that), with:

- a Start menu entry and, if you like, a desktop shortcut
- **Open in tacoterm** on folder and drive right-click menus
- tacoterm on your PATH, so `tacoterm` starts it from any console
- the console that passes pictures through, and tacoshell when it's built
  alongside (with an option to make it the default shell)

Uninstall from Settings → Apps. Your `config.toml` stays.

To build the installer yourself, after the build below:
`.\scripts\build-installer.ps1` (it fetches Inno Setup if needed) writes
`target\installer\tacoterm-<version>-setup.exe`.

`tacoterm --cwd DIR` (or `tacoterm DIR`) opens in that folder.

## Build (Windows)

```
cargo build --release
.\scripts\get-conpty.ps1
target\release\tacoterm.exe
```

`get-conpty.ps1` puts Windows Terminal's console (`conpty.dll` and
`OpenConsole.exe`, from Microsoft's ConPTY NuGet package) next to
`tacoterm.exe`. tacoterm uses it when it's there; the console built into
Windows drops picture sequences on the way, so without it everything but
pictures still works. The CI build bundles it.

The window and ConPTY are Windows-only. The terminal core (grid, VT
parser, renderer, config, key mapping) is a portable library, so
`cargo test` runs anywhere; the render test draws a frame offscreen and
checks its pixels (it skips when no GPU adapter, not even a software one,
is available). `TACOTERM_SNAPSHOT=frame.png cargo test --test render` saves
that frame (`TACOTERM_SNAPSHOT_TABS` saves the tabs-and-splits one,
`TACOTERM_SNAPSHOT_SETTINGS` the settings page,
`TACOTERM_SNAPSHOT_IMAGES` the pictures).

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
background_image = "~/Pictures/wall.jpg"
background_image_opacity = 0.25

shell = "auto"                   # PowerShell; or "cmd.exe", "wsl.exe ~", …
prompt = "arrow"                 # styled; "classic" = the shell's own
prompt_name = "LX"               # shown first; "" for none
prompt_path = "folder"           # full, folder or none
prompt_symbol = "λ"              # any text; "" for none
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
command_palette = "ctrl+shift+p"
quick_select = "ctrl+shift+space"
previous_prompt = "ctrl+shift+up"
```

The file tacoterm writes on first run lists every key and binding.

## Layout

| File | What it holds |
| --- | --- |
| `src/main.rs` | Entry point (Windows binary) |
| `src/app.rs` | winit event loop, tabs and panes, mouse/keyboard, find |
| `src/app/page.rs` | The settings page: input and live preview |
| `src/app/palette.rs` | Command palette and quick select |
| `src/menu.rs` | The right-click menu |
| `src/win.rs` | Win32 bits: summon hotkey, backdrop, links, beep |
| `src/pty.rs` | ConPTY wrapper: spawn, resize, write, exit watch |
| `src/lib.rs` | The portable terminal core |
| `src/vt.rs` | `vte` parser → grid |
| `src/grid.rs` | Screen, scrollback, reflow, selection, search, links |
| `src/input.rs` | Key events → VT bytes; keybinding matching |
| `src/mouse.rs` | Mouse reporting encodings |
| `src/layout.rs` | Split-pane tree |
| `src/graphics.rs` | Pictures: kitty graphics, sixel, iTerm2 images |
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
- [x] OSC 8 hyperlinks, dragging pane dividers, command palette, quick select
- [ ] Per-row damage tracking
- [ ] A shell of our own

## License

MIT, (c) 2026 tacobellerontop-sudo.
