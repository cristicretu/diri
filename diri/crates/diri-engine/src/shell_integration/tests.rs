use super::*;

fn integration() -> (tempfile::TempDir, ShellIntegration) {
    let dir = tempfile::tempdir().expect("tempdir");
    let integration = ShellIntegration::install(dir.path()).expect("install");
    (dir, integration)
}

fn value<'a>(spec: &'a PtySpec, name: &str) -> Option<&'a str> {
    last_value(&spec.env, name)
}

#[test]
fn zsh_gets_the_shim_and_keeps_an_inherited_zdotdir() {
    let (_dir, integration) = integration();
    let shim = integration.root.join("zsh").to_string_lossy().into_owned();

    let mut spec = PtySpec::new(vec!["/bin/zsh".into(), "-l".into()], "/tmp");
    integration.apply(&mut spec);
    assert_eq!(spec.argv, ["/bin/zsh", "-l"], "argv is unchanged");
    assert_eq!(value(&spec, "ZDOTDIR"), Some(shim.as_str()));
    assert_eq!(value(&spec, ZSH_ORIGINAL_ZDOTDIR), None);

    let mut spec = PtySpec::new(vec!["/bin/zsh".into(), "-l".into()], "/tmp")
        .env("ZDOTDIR", "/home/u/.config/zsh")
        .env(ZSH_ORIGINAL_ZDOTDIR, "/stale");
    integration.apply(&mut spec);
    assert_eq!(value(&spec, "ZDOTDIR"), Some(shim.as_str()));
    assert_eq!(
        value(&spec, ZSH_ORIGINAL_ZDOTDIR),
        Some("/home/u/.config/zsh")
    );
    assert_eq!(
        spec.env.iter().filter(|(key, _)| key == "ZDOTDIR").count(),
        1
    );

    // The shim's own value is never handed back as the user's.
    let mut spec = PtySpec::new(vec!["/bin/zsh".into(), "-l".into()], "/tmp").env("ZDOTDIR", &shim);
    integration.apply(&mut spec);
    assert_eq!(value(&spec, ZSH_ORIGINAL_ZDOTDIR), None);
}

#[test]
fn bash_gets_a_bootstrap_that_chains_an_inherited_prompt_command() {
    let (_dir, integration) = integration();
    let script = integration.root.join(BASH_SCRIPT);
    let boot = bash_bootstrap(&script).expect("quotable");
    assert_eq!(
        boot,
        format!(
            ". '{}'; builtin eval \"${{__diri_trap_install-}}\"",
            script.display()
        )
    );
    // diri.bash rebuilds the bootstrap from its own path to replace it.
    assert!(
        BASH.contains(r#"boot=". '${BASH_SOURCE[0]}'; builtin eval \"\${__diri_trap_install-}\"""#)
    );

    let mut spec = PtySpec::new(vec!["/usr/local/bin/bash".into(), "-l".into()], "/tmp");
    integration.apply(&mut spec);
    assert_eq!(spec.argv, ["/usr/local/bin/bash", "-l"]);
    assert_eq!(value(&spec, "PROMPT_COMMAND"), Some(boot.as_str()));

    let mut spec = PtySpec::new(vec!["/bin/bash".into(), "-l".into()], "/tmp")
        .env("PROMPT_COMMAND", "history -a");
    integration.apply(&mut spec);
    assert_eq!(
        value(&spec, "PROMPT_COMMAND"),
        Some(format!("{boot}\nhistory -a").as_str())
    );

    assert_eq!(bash_bootstrap(Path::new("/it's/diri.bash")), None);
}

#[test]
fn other_launches_are_left_alone() {
    let (_dir, integration) = integration();
    for argv in [
        vec!["/opt/homebrew/bin/fish", "-l"],
        vec!["/bin/sh", "-l"],
        vec!["/bin/zsh", "-lc", "make"],
        vec![
            "/bin/bash",
            "-i",
            "-l",
            "-c",
            "claude; exec /bin/bash -i -l",
        ],
        vec!["/bin/zsh"],
    ] {
        let mut spec = PtySpec::new(argv.iter().map(|arg| (*arg).into()).collect(), "/tmp")
            .env("HOME", "/home/u");
        let before = spec.clone();
        integration.apply(&mut spec);
        assert_eq!(spec.argv, before.argv, "{argv:?}");
        assert_eq!(spec.env, before.env, "{argv:?}");
    }
}

#[cfg(unix)]
#[test]
fn installed_scripts_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, integration) = integration();
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&integration.root), 0o700);
    assert_eq!(mode(&integration.root.join("zsh")), 0o700);
    assert_eq!(mode(&integration.root.join("zsh/.zshenv")), 0o600);
    assert_eq!(mode(&integration.root.join(BASH_SCRIPT)), 0o600);
    // A second install (every Engine start) is a no-op on identical files.
    ShellIntegration::install(integration.root.parent().unwrap()).expect("reinstall");
}

/// A real login shell on a PTY, as a terminal tab runs it.
#[cfg(unix)]
mod real_shells {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Every test prompt contains this, so the driver can tell when the
    /// shell is ready for the next line whether or not that prompt has marks.
    const PROMPT: &str = "PROMPT[";

    struct Shell {
        pty: crate::pty::Pty,
        writer: crate::pty::PtyStream,
        output: mpsc::Receiver<Vec<u8>>,
        seen: Vec<u8>,
    }

    impl Shell {
        fn start(spec: &PtySpec) -> Self {
            let pty = crate::pty::Pty::spawn(spec).expect("spawn shell");
            let mut reader = pty.reader().expect("reader");
            let writer = pty.writer().expect("writer");
            let (sender, output) = mpsc::channel();
            std::thread::spawn(move || {
                let mut buffer = [0_u8; 8192];
                while let Ok(read) = reader.read(&mut buffer) {
                    if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            });
            let mut shell = Self {
                pty,
                writer,
                output,
                seen: Vec::new(),
            };
            shell.wait_for_prompts(1);
            shell
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.seen).into_owned()
        }

        fn prompts(&self) -> usize {
            self.text().matches(PROMPT).count()
        }

        fn wait_for_prompts(&mut self, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while self.prompts() < count {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !left.is_zero(),
                    "no prompt {count}; output: {:?}",
                    self.text()
                );
                if let Ok(bytes) = self.output.recv_timeout(left) {
                    self.seen.extend(bytes);
                }
            }
            // The rest of the prompt (B follows the prompt text).
            while let Ok(bytes) = self.output.recv_timeout(Duration::from_millis(150)) {
                self.seen.extend(bytes);
            }
        }

        /// Types `line` and waits for the prompt after it.
        fn run(&mut self, line: &str) {
            let prompts = self.prompts();
            self.writer
                .write_all(format!("{line}\r").as_bytes())
                .expect("type");
            self.wait_for_prompts(prompts + 1);
        }

        /// The OSC 133 marks so far, without parameters other than D's status.
        fn marks(&self) -> Vec<String> {
            let text = self.text();
            text.split("\x1b]133;")
                .skip(1)
                .map(|rest| {
                    let mark = &rest[..rest.find('\x07').expect("BEL-terminated mark")];
                    mark.split(";click_events").next().unwrap().to_owned()
                })
                .collect()
        }

        fn finish(mut self) -> String {
            let _ = self.writer.write_all(b"exit\r");
            let _ = self.pty.terminate(Duration::from_secs(2));
            self.text()
        }
    }

    fn find_shell(name: &str) -> Option<String> {
        ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"]
            .iter()
            .map(|dir| format!("{dir}/{name}"))
            .find(|path| Path::new(path).is_file())
    }

    /// A clean login shell in `home`, as `session.spawn` would start it with
    /// the integration applied.
    fn spec(shell: &str, home: &Path, integration: &ShellIntegration) -> PtySpec {
        let mut spec = PtySpec::new(vec![shell.into(), "-l".into()], home)
            .env("HOME", &home.to_string_lossy())
            .env("SHELL", shell)
            .env("TERM", "xterm-256color")
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C")
            .env("BASH_SILENCE_DEPRECATION_WARNING", "1")
            .size(200, 50);
        integration.apply(&mut spec);
        spec
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// The marks each command line leaves, from its C to the next prompt's B:
    /// `C D;<status> A B`, or `A B` alone for an empty line.
    fn assert_command_marks(marks: &[String], expected: &[&[&str]]) {
        let tail: Vec<&str> = marks.iter().map(String::as_str).collect();
        let expected: Vec<&str> = expected
            .iter()
            .flat_map(|line| line.iter().copied())
            .collect();
        assert!(
            tail.ends_with(&expected),
            "marks {tail:?} do not end with {expected:?}"
        );
    }

    /// A powerlevel10k-shaped zsh setup: ZDOTDIR moved by ~/.zshenv, a prompt
    /// rebuilt in its own precmd from `$?`, and its own preexec hook.
    #[test]
    fn zsh_marks_commands_under_a_prompt_framework() {
        let Some(zsh) = find_shell("zsh") else {
            eprintln!("skipping: no zsh");
            return;
        };
        let (_dir, integration) = integration();
        let home = tempfile::tempdir().unwrap();
        write(
            &home.path().join(".zshenv"),
            "export ZDOTDIR=\"$HOME/zdot\"\nexport USER_ZSHENV=ran\n",
        );
        write(
            &home.path().join("zdot/.zshrc"),
            r#"
setopt prompt_subst
typeset -g fw_status fw_preexec=0
fw_precmd() { fw_status=$?; PROMPT='PROMPT[${fw_status}]> ' }
fw_preexec() { (( fw_preexec++ )) }
autoload -Uz add-zsh-hook
add-zsh-hook precmd fw_precmd
add-zsh-hook preexec fw_preexec
"#,
        );
        let mut shell = Shell::start(&spec(&zsh, home.path(), &integration));
        shell.run("echo hi");
        shell.run("false");
        shell.run("");
        shell.run("(exit 7)");
        shell.run(
            r#"print -r -- "zdotdir=$ZDOTDIR orig=${DIRI_ZSH_ZDOTDIR-none} env=$USER_ZSHENV login=$options[login] fw=$fw_preexec""#,
        );
        let marks = shell.marks();
        let text = shell.finish();

        assert_command_marks(
            &marks,
            &[
                &["C", "D;0", "A", "B"],
                &["C", "D;1", "A", "B"],
                &["A", "B"],
                &["C", "D;7", "A", "B"],
                &["C", "D;0", "A", "B"],
            ],
        );
        // The framework still saw each command's status and ran its hooks.
        assert!(text.contains("PROMPT[1]> "), "{text:?}");
        assert!(text.contains("PROMPT[7]> "), "{text:?}");
        let home = home.path().display();
        assert!(
            text.contains(&format!(
                "zdotdir={home}/zdot orig=none env=ran login=on fw=4"
            )),
            "startup files, login shell and ZDOTDIR as without Diri: {text:?}"
        );
    }

    /// A `ZDOTDIR` the shell inherits is where zsh reads every startup file.
    #[test]
    fn zsh_restores_an_inherited_zdotdir() {
        let Some(zsh) = find_shell("zsh") else {
            eprintln!("skipping: no zsh");
            return;
        };
        let (_dir, integration) = integration();
        let home = tempfile::tempdir().unwrap();
        let zdot = home.path().join("cfg");
        write(&zdot.join(".zshenv"), "export FROM_ZDOTDIR_ENV=1\n");
        write(&zdot.join(".zshrc"), "PS1='PROMPT[%?]> '\n");
        write(&home.path().join(".zshrc"), "PS1='WRONG> '\n");
        let mut spec = PtySpec::new(vec![zsh.clone(), "-l".into()], home.path())
            .env("HOME", &home.path().to_string_lossy())
            .env("ZDOTDIR", &zdot.to_string_lossy())
            .env("TERM", "xterm-256color")
            .env("PATH", "/usr/bin:/bin")
            .size(200, 50);
        integration.apply(&mut spec);
        let mut shell = Shell::start(&spec);
        shell.run(r#"print -r -- "z=$ZDOTDIR e=$FROM_ZDOTDIR_ENV o=${DIRI_ZSH_ZDOTDIR-none}""#);
        let marks = shell.marks();
        let text = shell.finish();
        assert!(
            text.contains(&format!("z={} e=1 o=none", zdot.display())),
            "{text:?}"
        );
        assert!(!text.contains("WRONG> "));
        // A static PS1 is marked from the very first prompt.
        assert_eq!(marks, ["A", "B", "C", "D;0", "A", "B"]);
    }

    /// The common hand-rolled bash setup: a profile appending to
    /// PROMPT_COMMAND with `;`, a prompt rebuilt there from `$?`, and the
    /// user's own DEBUG trap, which must keep seeing the real `$?`.
    #[test]
    fn bash_marks_commands_and_chains_prompt_command_and_debug_trap() {
        let Some(bash) = find_shell("bash") else {
            eprintln!("skipping: no bash");
            return;
        };
        let (_dir, integration) = integration();
        let home = tempfile::tempdir().unwrap();
        write(
            &home.path().join(".bash_profile"),
            r#"
my_prompt() { PS1="PROMPT[$?]> "; }
PROMPT_COMMAND="$PROMPT_COMMAND; my_prompt"
trap 'user_trap_status=$?' DEBUG
set -o nounset
"#,
        );
        let mut shell = Shell::start(&spec(&bash, home.path(), &integration));
        shell.run("echo hi");
        shell.run("false");
        shell.run("");
        shell.run("(exit 4); echo \"trap=$user_trap_status\"");
        shell.run(
            "shopt -q login_shell && echo login=yes; printenv PROMPT_COMMAND || echo unexported",
        );
        let marks = shell.marks();
        let text = shell.finish();

        assert_command_marks(
            &marks,
            &[
                &["C", "D;0", "A", "B"],
                &["C", "D;1", "A", "B"],
                &["A", "B"],
                &["C", "D;0", "A", "B"],
                &["C", "D;0", "A", "B"],
            ],
        );
        assert!(text.contains("PROMPT[1]> "), "{text:?}");
        assert!(text.contains("trap=4"), "user DEBUG trap saw $?: {text:?}");
        assert!(text.contains("login=yes"), "{text:?}");
        assert!(text.contains("unexported"), "{text:?}");
        assert!(!text.contains("syntax error"), "{text:?}");
        assert!(!text.contains("command not found"), "{text:?}");

        // The parser #748 added records the outcomes on the grid.
        let mut screen = diri_terminal_state::HeadlessScreen::new(200, 50);
        screen.feed(text.as_bytes());
        let styles: Vec<_> = screen
            .full_snapshot()
            .changed_rows
            .into_iter()
            .flat_map(|row| row.cells)
            .map(|cell| cell.style)
            .collect();
        let count = |bit| styles.iter().filter(|style| style.contains(bit)).count();
        assert_eq!(count(diri_proto::grid::TermStyle::COMMAND_FAILED), 1);
        assert_eq!(count(diri_proto::grid::TermStyle::COMMAND_SUCCEEDED), 3);
    }

    /// bash-preexec with a starship-style precmd that rebuilds PS1: the
    /// integration joins its hook arrays instead of chaining a DEBUG trap.
    #[test]
    fn bash_joins_bash_preexec_and_a_prompt_framework() {
        let Some(bash) = find_shell("bash") else {
            eprintln!("skipping: no bash");
            return;
        };
        let (_dir, integration) = integration();
        let home = tempfile::tempdir().unwrap();
        write(
            &home.path().join(".bash_profile"),
            "[ -r ~/.bashrc ] && . ~/.bashrc\n",
        );
        write(
            &home.path().join(".bashrc"),
            r#"
# A faithful sketch of bash-preexec's moving parts.
bash_preexec_imported=defined
precmd_functions=() preexec_functions=()
__bp_interactive=
__bp_set_ret() { return "$1"; }
__bp_precmd_invoke_cmd() {
    local status=$? f
    __bp_interactive=
    for f in "${precmd_functions[@]}"; do __bp_set_ret "$status"; "$f"; done
}
__bp_interactive_mode() { __bp_interactive=on; }
__bp_preexec_invoke_exec() {
    [[ -n "$__bp_interactive" && -z "${COMP_LINE-}" ]] || return 0
    # Like bash-preexec's __bp_in_prompt_command: an empty line runs only
    # PROMPT_COMMAND, which is no command of the user's.
    [[ $'\n'"$PROMPT_COMMAND"$'\n' != *$'\n'"$BASH_COMMAND"$'\n'* ]] || return 0
    __bp_interactive=
    local f
    for f in "${preexec_functions[@]}"; do "$f" "$BASH_COMMAND"; done
}
PROMPT_COMMAND=$'__bp_precmd_invoke_cmd\n'"$PROMPT_COMMAND"$'\n__bp_interactive_mode'
trap '__bp_preexec_invoke_exec' DEBUG
starship_precmd() { PS1="PROMPT[$?]> "; }
starship_preexec() { STARSHIP_START=1; }
precmd_functions+=(starship_precmd)
preexec_functions+=(starship_preexec)
set -o nounset
"#,
        );
        let mut shell = Shell::start(&spec(&bash, home.path(), &integration));
        shell.run("echo hi");
        shell.run("false");
        shell.run("");
        shell.run("echo \"start=$STARSHIP_START\"; trap -p DEBUG");
        let marks = shell.marks();
        let text = shell.finish();

        assert_command_marks(
            &marks,
            &[
                &["C", "D;0", "A", "B"],
                &["C", "D;1", "A", "B"],
                &["A", "B"],
                &["C", "D;0", "A", "B"],
            ],
        );
        assert!(text.contains("PROMPT[1]> "), "{text:?}");
        assert!(text.contains("start=1"), "{text:?}");
        assert!(
            text.contains("trap -- '__bp_preexec_invoke_exec' DEBUG"),
            "bash-preexec keeps the DEBUG trap: {text:?}"
        );
    }
}
