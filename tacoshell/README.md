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
  `NAME=value` or `set NAME=value` sets one; `NAME=value cmd` sets it just
  for cmd
- Wildcards `*.txt`, `src/**/*.rs`, `file?.log`; braces `{a,b}`; command
  output `$(cmd)`; arithmetic `$((x * 2))` and `math`; `!!` and `!$`
- Blocks, fish style, on one line or several: `if … else if … else … end`,
  `for x in …; … end`, `while …; … end`, `function name; … end` (with
  `$1`, `$#`, `$argv`, `return`), `break`, `continue`, `not` / `!`,
  `test` / `[ ]`, `read`
- Programs from PATH using PATHEXT (`.exe`, `.cmd`, `.bat`, …); `.ps1`
  scripts run through PowerShell; cmd's own commands (`dir`, `copy`, `del`,
  `type`, `mklink`, `start`, …) run through `cmd /c`
- Folders: `z part-of-name` jumps to the folder you use most that matches;
  `cd name` falls back to it too; `..`, `...` or `dir/` alone goes there
- Built-ins: `cd` (`cd -`, `cd..`, `cd ...`, `D:`), `pushd`/`popd`/`dirs`, `ls` (a
  TYPE | NAME | SIZE table with folder totals; `ls -1` for names only),
  `pwd`, `echo`, `set`, `export`, `unset`, `env`, `alias`, `unalias`,
  `which`/`type`, `functions`, `history`, `clear`/`cls`, `source`, `help`,
  `exit`
- The everyday Unix commands, built in so they're on every machine: `cat`,
  `head`, `tail`, `wc`, `grep` (regex), `sort`, `uniq`, `cut`, `tr`, `seq`,
  `tee`, `find`, `mkdir`, `touch`, `cp`, `mv`, `rm`, `du`, `df`, `ps`,
  `kill`, `date`, `time`, `basename`, `dirname`, `realpath`, `sleep`,
  `open`. They read files, or a pipe or `< file` when given none
- In the terminal, `cat`, `head`, `tail` and `grep` draw their text in a
  box so it stands apart from what you typed; piped or redirected, the text
  stays plain
- Pictures: `img photo.png` shows one with the kitty graphics protocol
  (`-k`), sixel (`-s`) or colored half-blocks that work in any terminal
  (`-b`). It picks kitty where the terminal is known to show it;
  `TACOSHELL_IMG=kitty|sixel|blocks` overrides
- Line editing: colors as you type (commands green, or red when there's no
  such command), a gray suggestion from history (Right or End takes it),
  Ctrl+R history search, Tab completes commands and paths, Up/Down history
  (kept in `%APPDATA%\tacoshell\history`), Ctrl+Left/Right, Ctrl+W/U/K,
  Esc, Ctrl+L, Ctrl+C, Ctrl+D; an unfinished line (open block, quote or
  trailing `|`) continues on the next
- The prompt shows the folder, the git branch and how long a slow command
  took. `TACOSHELL_PROMPT='{cyan}{user}{reset} {cwd}{git} {arrow} '`
  changes it (see `help`), or define a `prompt` function. It also reports
  the folder to the terminal (OSC 9;9), so tacoterm's new tabs open where
  you are
- `%APPDATA%\tacoshell\init.tsh` runs at startup (aliases and such);
  `tacoshell -c "line"` and `tacoshell file.tsh` run commands and exit

## Not yet

Background jobs (`cmd &` runs in sequence, as in cmd), local variables
(variables are environment variables), and `ls -l`.

It also builds on Linux and macOS, so `cargo test` runs anywhere.
