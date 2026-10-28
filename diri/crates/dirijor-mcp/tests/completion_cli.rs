#![cfg(unix)]

//! `dirijor completion fish|zsh|bash` produces scripts each shell accepts
//! and that complete commands, subcommands, flags, flag values and live
//! session ids. The session lookup is stubbed, so no Engine is needed. A
//! shell that is not installed is skipped.

use std::path::{Path, PathBuf};
use std::process::Command;

fn script(shell: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .args(["completion", shell])
        .output()
        .unwrap();
    assert!(output.status.success(), "{shell}");
    String::from_utf8(output.stdout).unwrap()
}

fn shell(name: &str) -> Option<PathBuf> {
    ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"]
        .iter()
        .map(|directory| Path::new(directory).join(name))
        .find(|path| path.is_file())
}

fn run(shell: &Path, harness: &str, script: &str) -> String {
    let temp = tempfile::tempdir().unwrap();
    let script_path = temp.path().join("completion");
    std::fs::write(&script_path, script).unwrap();
    let harness_path = temp.path().join("harness");
    std::fs::write(&harness_path, harness).unwrap();
    let mut command = Command::new(shell);
    if shell.ends_with("zsh") {
        command.arg("-f");
    }
    let output = command
        .arg(&harness_path)
        .arg(&script_path)
        .env("HOME", temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}: {}",
        shell.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

const STUB_SESSIONS: &str = "printf 's_alpha\\tAlpha\\ns_beta\\tBeta\\n'";

#[test]
fn bash_completes_commands_flags_values_and_sessions() {
    let bash = shell("bash").expect("bash");
    let harness = format!(
        "source \"$1\"\n\
         dirijor() {{ {STUB_SESSIONS}; }}\n\
         t() {{ COMP_WORDS=(\"$@\"); COMP_CWORD=$((${{#COMP_WORDS[@]}}-1)); COMPREPLY=(); _dirijor; echo \"${{COMPREPLY[*]}}\"; }}\n\
         t dirijor se\n\
         t dirijor session re\n\
         t dirijor send-key s_alpha ctrl+\n\
         t dirijor wait s_a --until ex\n\
         t dirijor session ls --\n\
         t dirijor kill s_\n\
         t dirijor run --cwd /tmp --sp\n\
         t dirijor completion ''\n"
    );
    let lines: Vec<String> = run(&bash, &harness, &script("bash"))
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        lines,
        [
            "send send-key session",
            "rename read release reconnect reset-terminal",
            "ctrl+c ctrl+d ctrl+z ctrl+l ctrl+r",
            "exited",
            "--all --status --json",
            "s_alpha s_beta",
            "--split",
            "fish zsh bash",
        ]
    );
}

#[test]
fn zsh_completes_with_descriptions() {
    let Some(zsh) = shell("zsh") else {
        eprintln!("zsh not installed; skipped");
        return;
    };
    let harness = format!(
        "compdef() {{ :; }}\n\
         source $1\n\
         dirijor() {{ {STUB_SESSIONS}; }}\n\
         _describe() {{ local arr=$4; print -r -- \"$3: ${{(P)arr}}\"; }}\n\
         compadd() {{ shift; print -r -- \"add: $*\"; }}\n\
         _files() {{ print -r -- files; }}\n\
         t() {{ words=(\"$@\"); CURRENT=${{#words}}; PREFIX=${{words[CURRENT]}}; _dirijor; }}\n\
         t dirijor kill ''\n\
         t dirijor wait s_a --until ''\n\
         t dirijor wait --\n\
         t dirijor worktree list ''\n"
    );
    let out = run(&zsh, &harness, &script("zsh"));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "session: s_alpha:Alpha s_beta:Beta");
    assert_eq!(
        lines[1],
        "add: done idle working needsInput blocked exited starting unknown"
    );
    assert!(lines[2].starts_with("option: --for:Give up after"), "{out}");
    assert_eq!(lines[3], "files");
}

#[test]
fn fish_completes_commands_and_sessions() {
    let Some(fish) = shell("fish") else {
        eprintln!("fish not installed; skipped");
        return;
    };
    let harness = format!(
        "source $argv[1]\n\
         function dirijor; {STUB_SESSIONS}; end\n\
         for line in 'dirijor se' 'dirijor session re' 'dirijor kill s_' 'dirijor zoom s_alpha --of'\n  \
           echo (complete -C \"$line\" | string replace -r '\\t.*' '' | string join ' ')\n\
         end\n"
    );
    let lines: Vec<String> = run(&fish, &harness, &script("fish"))
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        lines,
        [
            "send send-key session",
            "read reconnect release rename reset-terminal",
            "s_alpha s_beta",
            "--off",
        ]
    );
}

#[test]
fn unknown_shells_and_an_absent_engine_fail_gently() {
    let output = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .args(["completion", "powershell"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("fish, zsh, bash"));
    // The scripts call this on every TAB: silent and successful offline.
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dirijor"))
        .env("DIRIJOR_SOCKET", temp.path().join("missing.sock"))
        .args(["__complete", "targets"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}
