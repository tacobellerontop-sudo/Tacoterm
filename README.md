# tacoterm

A GPU-accelerated terminal emulator for Windows, written in Rust.

The real console lives in a Windows pseudo-console (ConPTY); tacoterm is a fast
window onto it. `cmd.exe`, PowerShell and WSL all just work, because from their
side nothing is unusual — they talk to a console, as always.

## Status: scaffold

The bones are real: ConPTY spawn/resize, a VT parser feeding a grid with
scrollback, keyboard mapping, TOML config, and the winit event loop with a PTY
reader thread. The renderer (`src/renderer.rs`) is stubbed — that is the next
build.

## Build (Windows only)

```
cargo run --release
```

tacoterm drives ConPTY, so it only builds on Windows. First run writes a starter
config to `%APPDATA%\tacoterm\config.toml`.

## Layout

| File | What it holds |
| --- | --- |
| `src/main.rs` | Entry point, module wiring |
| `src/app.rs` | winit event loop, PTY reader thread, redraw plumbing |
| `src/pty.rs` | ConPTY wrapper: spawn, resize, read, write |
| `src/vt.rs` | `vte` parser → grid (cursor moves, erasing, SGR colors) |
| `src/grid.rs` | Cell buffer, scrollback, pen state |
| `src/input.rs` | Key events → VT byte sequences |
| `src/renderer.rs` | GPU text rendering (**TODO**: wgpu + cosmic-text) |
| `src/config.rs` | TOML config, written on first run |

## Roadmap

- [x] ConPTY spawn / resize / teardown
- [x] VT parsing: printing, cursor movement, erase, SGR
- [x] Grid + scrollback + config
- [ ] Renderer: glyph atlas, damage-tracked repaints
- [ ] Tabs, splits, find in scrollback
- [ ] Themes, quake-mode dropdown, URL hints

## License

MIT, (c) 2026 tacobellerontop-sudo.
