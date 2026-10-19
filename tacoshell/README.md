# tacoshell

A native command shell for Windows, written in Rust: the companion to
tacoterm. It starts fast, keeps Windows paths as they are (backslash is just
a path separator), and speaks a small, familiar syntax.

```
cargo run -p tacoshell
```

`cargo build --release --workspace` puts `tacoshell.exe` beside
`tacoterm.exe`; tacoterm then offers it under right-click → shell and on the
settings page (or set `shell = "tacoshell.exe"` in `config.toml`).

## What works

- Pipes `a | b`, `a && b`, `a || b`, `a ; b` (and cmd's `a & b`)
- Redirection: `> f`, `>> f`, `< f`, `2> f`, `2>> f`, `2>&1`
- Quotes: `'as is'`, `"expands $VAR"`; the escape character is a backtick
- Variables: `$VAR`, `${VAR}`, `%VAR%`, `$?`; `~` is your home folder;
  `NAME=value` or `set NAME=value` sets one
- Programs from PATH using PATHEXT (`.exe`, `.cmd`, `.bat`, …); `.ps1`
  scripts run through PowerShell; cmd's own commands (`dir`, `copy`, `del`,
  `type`, `mklink`, `start`, …) run through `cmd /c`
- Built-ins: `cd` (`cd -`, `cd..`, `D:`), `ls`, `pwd`, `echo`, `set`,
  `export`, `unset`, `env`, `alias`, `unalias`, `which`, `history`,
  `clear`/`cls`, `source`, `help`, `exit`
- Line editing: Tab completes commands and paths, Up/Down history (kept in
  `%APPDATA%\tacoshell\history`), Ctrl+Left/Right, Ctrl+W/U/K, Esc,
  Ctrl+L, Ctrl+C, Ctrl+D
- The prompt reports the folder to the terminal (OSC 9;9), so tacoterm's
  new tabs open where you are
- `%APPDATA%\tacoshell\init.tsh` runs at startup (aliases and such);
  `tacoshell -c "line"` and `tacoshell file.tsh` run commands and exit

## Not yet

Globbing (`*.txt`), command substitution, `if`/loops/functions, background
jobs, and a configurable prompt.

It also builds on Linux and macOS, so `cargo test` runs anywhere.
