//! Runs the real tacoshell binary. The only outside program these lean on is
//! tacoshell itself (`$TS`), which reads commands from a pipe, so they pass
//! the same on Windows and elsewhere.

use std::path::PathBuf;
use std::process::{Command, Output};

const TS: &str = env!("CARGO_BIN_EXE_tacoshell");

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tacoshell-run-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_in(dir: &std::path::Path, line: &str) -> Output {
    Command::new(TS)
        .args(["-c", line])
        .current_dir(dir)
        .env("TS", TS)
        .output()
        .unwrap()
}

fn run(line: &str) -> Output {
    run_in(&std::env::temp_dir(), line)
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).replace("\r\n", "\n")
}

#[test]
fn sequences_and_conditions() {
    let o = run("echo one && echo two; false && echo no || echo three & echo four");
    assert_eq!(stdout(&o), "one\ntwo\nthree\nfour\n");
}

#[test]
fn variables() {
    let o = run("X=taco; echo $X %X% ${X}s \"$X shell\" '$X' $?; false; echo $?");
    assert_eq!(stdout(&o), "taco taco tacos taco shell $X 0\n1\n");
}

#[test]
fn exit_codes() {
    assert_eq!(run("exit 3").status.code(), Some(3));
    let o = run("no-such-program-here");
    assert_eq!(o.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&o.stderr).contains("command not found"));
    assert_eq!(
        run("echo x | no-such-program-here").status.code(),
        Some(127)
    );
}

#[test]
fn pipes_into_programs() {
    // The second tacoshell reads lines from the pipe and runs them.
    let o = run("echo echo piped | \"$TS\"");
    assert_eq!(stdout(&o), "piped\n");
    let o = run("echo echo a | \"$TS\" | \"$TS\" -c 'echo b'");
    assert_eq!(stdout(&o), "b\n");
}

#[test]
fn redirections() {
    let dir = scratch("redir");
    let o = run_in(
        &dir,
        "echo hello > out.txt; echo again >> out.txt; echo echo from-file > in.txt",
    );
    assert!(o.status.success());
    let text = std::fs::read_to_string(dir.join("out.txt")).unwrap();
    assert_eq!(text.replace("\r\n", "\n"), "hello\nagain\n");
    assert_eq!(stdout(&run_in(&dir, "\"$TS\" < in.txt")), "from-file\n");

    run_in(
        &dir,
        "\"$TS\" -c nope-not-here 2> err.txt; \"$TS\" -c nope-not-here > both.txt 2>&1",
    );
    for file in ["err.txt", "both.txt"] {
        let text = std::fs::read_to_string(dir.join(file)).unwrap();
        assert!(text.contains("command not found"), "{file}: {text:?}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cd_and_scripts() {
    let dir = scratch("script");
    std::fs::create_dir_all(dir.join("sub dir")).unwrap();
    std::fs::write(
        dir.join("script.tsh"),
        "# a comment\nalias greet='echo hi from'\ncd 'sub dir'\ngreet $X\nls -1 ..\nexit 4\necho not reached\n",
    )
    .unwrap();
    let o = Command::new(TS)
        .arg("script.tsh")
        .current_dir(&dir)
        .env("X", "script")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(4));
    assert_eq!(stdout(&o), "hi from script\nscript.tsh\nsub dir\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn parse_errors() {
    let o = run("echo 'unclosed");
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("missing closing"));
}

#[test]
fn builtins_read_pipes_and_files() {
    let dir = scratch("text");
    let o = run_in(
        &dir,
        "echo cherry > f.txt; echo apple >> f.txt; echo banana >> f.txt; \
         cat f.txt | sort | head -n 2; grep -c an < f.txt; cat f.txt | grep -v e | wc -l; \
         mkdir -p a/b && touch a/b/x && ls -1 a/b && rm -r a && ls -1",
    );
    assert_eq!(stdout(&o), "apple\nbanana\n1\n1\nx\nf.txt\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn wildcards_braces_and_substitution() {
    let dir = scratch("glob");
    std::fs::create_dir_all(dir.join("src/deep")).unwrap();
    for f in ["a.txt", "b.txt", "c.md", "src/x.rs", "src/deep/y.rs"] {
        std::fs::write(dir.join(f), "").unwrap();
    }
    let o = run_in(
        &dir,
        "echo *.txt; echo **/*.rs; echo '*.txt' none*; echo f.{a,b{1,2}}; \
         echo $(echo one; echo two) \"$(seq 2)\"; echo $((6 * 7)) $(( (1 + 2) * $(echo 3) ))",
    );
    assert_eq!(
        stdout(&o).replace('\\', "/"),
        "a.txt b.txt\nsrc/deep/y.rs src/x.rs\n*.txt none*\nf.a f.b1 f.b2\none two 1\n2\n42 9\n"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn blocks_and_functions() {
    let o = run(
        "function greet\n  echo \"hi $1 ($# args)\"\n  return 3\nend\n\
         greet bob x; echo status $?\n\
         for n in 1 2 3 4; if test $n = 2; continue; else if [ $n -eq 4 ]; break; end; echo n$n; end\n\
         set i=0; while test $i -lt 3; set i=$((i + 1)); end; echo i=$i\n\
         not false && echo negated\n\
         function fact; if test $1 -le 1; echo 1; else; echo $(( $1 * $(fact $(( $1 - 1 ))) )); end; end; fact 6\n\
         greet loud | tr a-z A-Z\n\
         T=temp \"$TS\" -c 'echo $T'; echo \"after: [$T]\"",
    );
    assert_eq!(
        stdout(&o),
        "hi bob (2 args)\nstatus 3\nn1\nn3\ni=3\nnegated\n720\nHI LOUD (1 ARGS)\ntemp\nafter: []\n"
    );
}

#[test]
fn scripts_span_lines() {
    let o = Command::new(TS)
        .env("TS", TS)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(
                b"for x in a b\n  echo $x |\n    tr a-z A-Z\nend\necho \"two\nlines\"\n",
            )?;
            child.wait_with_output()
        })
        .unwrap();
    assert_eq!(stdout(&o), "A\nB\ntwo\nlines\n");
}
