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
- `config.toml`: font, size, padding, theme, keybindings, default shell
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

## Config

First run writes a starter config to `%APPDATA%\tacoterm\config.toml`.
Every key is optional; delete a line to get its default back. A broken
config shows a warning and falls back to defaults.

```toml
font_family = "Cascadia Mono"   # falls back to Consolas, then others
font_size = 14.0
shell = "pwsh.exe -NoLogo"      # any command line: "wsl.exe ~", …

[theme]
background = "#0c0c0c"
foreground = "#cccccc"
palette = [ "#0c0c0c", "#c50f1f", … ]   # 16 colors

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
| `src/config.rs` | TOML config, written on first run |

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
