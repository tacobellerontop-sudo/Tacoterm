# tacoterm

A GPU-accelerated terminal emulator for Windows, written in Rust.

The real console lives in a Windows pseudo-console (ConPTY); tacoterm is a fast
window onto it. `cmd.exe`, PowerShell and WSL all just work, because from their
side nothing is unusual — they talk to a console, as always.

## Status: MVP

One resizable window onto `cmd.exe`, PowerShell or WSL, drawn on the GPU:

- ConPTY spawn / resize / teardown; the window closes when the shell exits
- VT parsing for what ConPTY and full-screen apps emit: cursor movement,
  erase, insert/delete, scroll regions, alternate screen, SGR (16 / 256 /
  truecolor), bold / italic / underline / strikethrough, titles, status
  reports, bracketed paste, application cursor keys
- Wide characters (CJK, emoji) and font fallback via cosmic-text
- Scrollback (mouse wheel, Shift+PgUp/PgDn, Ctrl+Shift+Home/End)
- Copy / paste: drag to select; Ctrl+Shift+C / Ctrl+Shift+V; right-click
  copies a selection or pastes, like conhost
- Built-in color schemes, cursor styles and a colored cmd prompt
- `config.toml` settings that apply live when saved
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
that frame.

## Settings

First run writes `%APPDATA%\tacoterm\config.toml` (Ctrl+, opens it in
Notepad). Save it and colors, fonts, cursor and keybindings apply right
away; shell and prompt apply to the next window. Every key is optional, and
a mistake shows a warning and falls back to the default.

```toml
# tokyo-night, catppuccin-mocha, dracula, one-half-dark, gruvbox-dark,
# nord, solarized-dark, one-half-light, campbell
color_scheme = "tokyo-night"     # Ctrl+Shift+T previews the next one
cursor_style = "bar"             # block, bar or underline
font_family = "Cascadia Mono"    # falls back to Consolas, then others
font_size = 14.0
padding = 10

shell = "pwsh.exe -NoLogo"       # any command line: "wsl.exe ~", …
# cmd.exe's prompt: $P path, $G >, $_ newline, $E[…m colors; "" = C:\>
prompt = "$E[1;34m$P$E[0m $E[32m❯$E[0m "

[theme]                          # override single colors of the scheme
background = "#101010"

[keybindings]
copy = "ctrl+shift+c"
paste = "ctrl+shift+v"
```

## Layout

| File | What it holds |
| --- | --- |
| `src/main.rs` | Entry point (Windows binary) |
| `src/app.rs` | winit event loop, PTY threads, mouse/keyboard, clipboard |
| `src/pty.rs` | ConPTY wrapper: spawn, resize, write, exit watch |
| `src/lib.rs` | The portable terminal core |
| `src/vt.rs` | `vte` parser → grid |
| `src/grid.rs` | Screen, scrollback, alternate screen, selection |
| `src/input.rs` | Key events → VT bytes; keybinding matching |
| `src/renderer.rs` | wgpu renderer with a cosmic-text glyph atlas |
| `src/config.rs` | TOML settings, written on first run |
| `src/schemes.rs` | Built-in color schemes |

## Roadmap

- [x] ConPTY spawn / resize / teardown
- [x] VT parsing, grid, scrollback, alternate screen
- [x] Renderer: glyph atlas, instanced quads, redraw only on change
- [x] Selection, copy / paste, config file, keybindings
- [ ] Per-row damage tracking, built-in box-drawing glyphs
- [ ] Reflow on resize, mouse reporting, OSC 52 / OSC 8
- [ ] Tabs, splits, find in scrollback
- [ ] Themes, quake-mode dropdown, URL hints

## License

MIT, (c) 2026 tacobellerontop-sudo.
