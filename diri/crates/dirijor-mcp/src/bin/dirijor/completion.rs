//! `dirijor completion fish|zsh|bash`: completion scripts generated from the
//! command tree in `spec.rs`, and `dirijor __complete targets`, which the
//! scripts call for live session ids.

use super::spec::{self, Command, Complete};
use super::*;

pub(super) fn run(arguments: &[String]) -> Result<(), CliError> {
    let shell = match arguments {
        [shell] => shell.as_str(),
        _ => {
            return Err(CliError::failure(
                "usage: dirijor completion fish|zsh|bash\n  \
                 fish: dirijor completion fish > ~/.config/fish/completions/dirijor.fish\n  \
                 zsh:  dirijor completion zsh > \"${fpath[1]}/_dirijor\"  (or: source <(dirijor completion zsh))\n  \
                 bash: dirijor completion bash > ~/.local/share/bash-completion/completions/dirijor",
            ));
        }
    };
    let script = script(shell).ok_or_else(|| {
        CliError::failure(format!(
            "unknown shell {shell:?}; choose {}",
            spec::SHELLS.join(", ")
        ))
    })?;
    print!("{script}");
    Ok(())
}

pub(super) fn script(shell: &str) -> Option<String> {
    let model = Model::new();
    match shell {
        "fish" => Some(fish(&model)),
        "zsh" => Some(zsh(&model)),
        "bash" => Some(bash(&model)),
        _ => None,
    }
}

/// `dirijor __complete targets`: `id<TAB>title` per live session. Silent
/// and quick on any failure, since a shell is waiting on it.
pub(super) fn complete(arguments: &[String]) -> Result<(), CliError> {
    if arguments.first().map(String::as_str) != Some("targets") {
        return Ok(());
    }
    let Ok(value) = request(Method::SESSION_LIST, json!({}), Duration::from_secs(1)) else {
        return Ok(());
    };
    let Ok(list) = serde_json::from_value::<SessionListResult>(value) else {
        return Ok(());
    };
    let mut out = io::stdout().lock();
    for record in list.sessions.iter().filter(|record| !record.is_archived()) {
        let title: String = record
            .title
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .take(60)
            .collect();
        let _ = writeln!(out, "{}\t{title}", record.id.0);
    }
    Ok(())
}

/// One completable command: every spelling of its word path.
struct Node {
    spellings: Vec<String>,
    command: &'static Command,
}

struct Model {
    nodes: Vec<Node>,
    /// Flags anywhere in the tree that take a value, so the path scanner
    /// skips their values.
    value_flags: Vec<String>,
}

impl Model {
    fn new() -> Self {
        fn walk(command: &'static Command, prefixes: &[String], nodes: &mut Vec<Node>) {
            for sub in command.commands.iter().filter(|sub| !sub.internal) {
                let spellings: Vec<String> = prefixes
                    .iter()
                    .flat_map(|prefix| {
                        std::iter::once(sub.name)
                            .chain(sub.aliases.iter().copied())
                            .map(move |name| {
                                if prefix.is_empty() {
                                    name.to_owned()
                                } else {
                                    format!("{prefix} {name}")
                                }
                            })
                    })
                    .collect();
                nodes.push(Node {
                    spellings: spellings.clone(),
                    command: sub,
                });
                walk(sub, &spellings, nodes);
            }
        }
        let mut nodes = vec![Node {
            spellings: vec![String::new()],
            command: &spec::ROOT,
        }];
        walk(&spec::ROOT, &[String::new()], &mut nodes);
        let mut value_flags: Vec<String> = nodes
            .iter()
            .flat_map(|node| node.command.flags)
            .filter(|flag| flag.value.is_some())
            .flat_map(|flag| {
                std::iter::once(flag.long.to_owned()).chain(flag.short.map(|ch| format!("-{ch}")))
            })
            .collect();
        value_flags.sort();
        value_flags.dedup();
        Self { nodes, value_flags }
    }

    fn command_paths(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .flat_map(|node| node.spellings.iter().map(String::as_str))
            .filter(|path| !path.is_empty())
            .collect()
    }
}

fn subcommands(command: &Command) -> impl Iterator<Item = (&'static str, &'static str)> + '_ {
    command
        .commands
        .iter()
        .filter(|sub| !sub.internal)
        .flat_map(|sub| {
            std::iter::once(sub.name)
                .chain(sub.aliases.iter().copied())
                .map(move |name| (name, sub.summary))
        })
}

/// Single-quoted for POSIX shells and fish alike.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "'\\''"))
}

/// Single-quoted for fish, which escapes `'` and `\` inside quotes.
fn fish_quote(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn fish(model: &Model) -> String {
    let mut out = String::from(
        "# dirijor completions for fish. Generated by `dirijor completion fish`.\n\
         # Install: dirijor completion fish > ~/.config/fish/completions/dirijor.fish\n\n",
    );
    out.push_str("set -g __dirijor_commands");
    for path in model.command_paths() {
        out.push(' ');
        out.push_str(&fish_quote(path));
    }
    out.push_str("\nset -g __dirijor_value_flags");
    for flag in &model.value_flags {
        out.push(' ');
        out.push_str(&fish_quote(flag));
    }
    out.push_str(
        "\n\n\
function __dirijor_path
    set -l words (commandline -opc)
    set -e words[1]
    set -l path
    set -l skip 0
    for word in $words
        if test $skip = 1
            set skip 0
            continue
        end
        if contains -- $word $__dirijor_value_flags
            set skip 1
            continue
        end
        if string match -q -- '-*' $word
            continue
        end
        set -l key (string join ' ' $path $word)
        if contains -- $key $__dirijor_commands
            set path $path $word
        else
            break
        end
    end
    string join ' ' $path
end

function __dirijor_at
    set -l path (__dirijor_path)
    test \"$path\" = \"$argv[1]\"
end

complete -c dirijor -f
",
    );
    for node in &model.nodes {
        for spelling in &node.spellings {
            let at = format!(
                "-n {}",
                fish_quote(&format!("__dirijor_at {}", quote(spelling)))
            );
            for (name, summary) in subcommands(node.command) {
                out.push_str(&format!(
                    "complete -c dirijor {at} -a {} -d {}\n",
                    fish_quote(name),
                    fish_quote(summary)
                ));
            }
            for flag in node.command.flags {
                let mut line = format!(
                    "complete -c dirijor {at} -l {}",
                    flag.long.trim_start_matches("--")
                );
                if let Some(short) = flag.short {
                    line.push_str(&format!(" -s {short}"));
                }
                if flag.value.is_some() {
                    if flag.choices.is_empty() {
                        line.push_str(" -r");
                        if flag.long == "--cwd" {
                            line.push_str(" -F");
                        }
                    } else {
                        line.push_str(&format!(" -x -a {}", fish_quote(&flag.choices.join(" "))));
                    }
                }
                line.push_str(&format!(" -d {}\n", fish_quote(flag.help)));
                out.push_str(&line);
            }
            let positional = match node.command.complete {
                Complete::Nothing => None,
                Complete::Target => {
                    Some("-a '(dirijor __complete targets 2>/dev/null)'".to_owned())
                }
                Complete::Path => Some("-F".to_owned()),
                Complete::Shell => Some(format!("-a {}", fish_quote(&spec::SHELLS.join(" ")))),
            };
            if let Some(positional) = positional {
                out.push_str(&format!("complete -c dirijor {at} {positional}\n"));
            }
            if node.command.name == "send-key" {
                out.push_str(&format!(
                    "complete -c dirijor {at} -a {}\n",
                    fish_quote(&spec::KEY_NAMES.join(" "))
                ));
            }
        }
    }
    out
}

/// The `case` patterns that recognise a command path, shared by zsh and bash.
fn path_case(model: &Model) -> String {
    model
        .command_paths()
        .iter()
        .map(|path| quote(path))
        .collect::<Vec<_>>()
        .join("|")
}

fn value_flag_case(model: &Model) -> String {
    model
        .value_flags
        .iter()
        .map(|flag| quote(flag))
        .collect::<Vec<_>>()
        .join("|")
}

fn positional_word(complete: Complete, command: &Command) -> &'static str {
    if command.name == "send-key" {
        return "target-key";
    }
    match complete {
        Complete::Nothing => "",
        Complete::Target => "target",
        Complete::Path => "path",
        Complete::Shell => "shell",
    }
}

fn zsh(model: &Model) -> String {
    let mut out = String::from(
        "#compdef dirijor\n\
         # dirijor completions for zsh. Generated by `dirijor completion zsh`.\n\
         # Install: dirijor completion zsh > \"${fpath[1]}/_dirijor\", or source <(dirijor completion zsh)\n\n\
         _dirijor() {\n  \
           local cmdpath='' word key i skip=0 pos=''\n  \
           local -a subs flags\n  \
           for (( i = 2; i < CURRENT; i++ )); do\n    \
             word=${words[i]}\n    \
             if (( skip )); then skip=0; continue; fi\n    \
             case $word in\n      ",
    );
    out.push_str(&format!(
        "({}) skip=1; continue ;;\n      (-*) continue ;;\n    esac\n    \
         key=${{cmdpath:+$cmdpath }}$word\n    \
         case $key in\n      ({}) cmdpath=$key ;;\n      (*) break ;;\n    esac\n  done\n  \
         case $cmdpath in\n",
        value_flag_case(model),
        path_case(model)
    ));
    let mut choices = String::new();
    for node in &model.nodes {
        let patterns = node
            .spellings
            .iter()
            .map(|spelling| quote(spelling))
            .collect::<Vec<_>>()
            .join("|");
        let subs = subcommands(node.command)
            .map(|(name, summary)| quote(&format!("{name}:{summary}")))
            .collect::<Vec<_>>()
            .join(" ");
        let flags = node
            .command
            .flags
            .iter()
            .flat_map(|flag| {
                std::iter::once(flag.long.to_owned())
                    .chain(flag.short.map(|ch| format!("-{ch}")))
                    .map(move |spelling| quote(&format!("{spelling}:{}", flag.help)))
            })
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!(
            "    ({patterns}) subs=({subs}); flags=({flags}); pos={} ;;\n",
            quote(positional_word(node.command.complete, node.command))
        ));
        for flag in node
            .command
            .flags
            .iter()
            .filter(|flag| !flag.choices.is_empty())
        {
            for spelling in &node.spellings {
                choices.push_str(&format!(
                    "    ({}) compadd -- {}; return ;;\n",
                    quote(&format!("{spelling} {}", flag.long)),
                    flag.choices.join(" ")
                ));
            }
        }
    }
    out.push_str("  esac\n  case \"$cmdpath ${words[CURRENT-1]}\" in\n");
    out.push_str(&choices);
    out.push_str(&format!(
        "  esac\n  \
         if [[ $PREFIX == -* ]]; then\n    _describe -t options option flags\n    return\n  fi\n  \
         (( $#subs )) && _describe -t commands command subs\n  \
         case $pos in\n    \
           (target|target-key)\n      \
             local -a targets\n      \
             targets=(${{(f)\"$(dirijor __complete targets 2>/dev/null | sed -e 's/:/\\\\:/g' -e \"s/$(printf '\\t')/:/\")\"}})\n      \
             (( $#targets )) && _describe -t sessions session targets\n      \
             [[ $pos == target-key ]] && compadd -- {keys} ;;\n    \
           (key) compadd -- {keys} ;;\n    \
           (edge) compadd -- {edges} ;;\n    \
           (shell) compadd -- {shells} ;;\n    \
           (path) _files ;;\n  \
         esac\n\
         }}\n\n\
         if [[ ${{zsh_eval_context[-1]}} == loadautofunc ]]; then\n  _dirijor \"$@\"\nelse\n  compdef _dirijor dirijor\nfi\n",
        keys = spec::KEY_NAMES.join(" "),
        edges = spec::EDGES.join(" "),
        shells = spec::SHELLS.join(" "),
    ));
    out
}

fn bash(model: &Model) -> String {
    let mut out = String::from(
        "# dirijor completions for bash (3.2 and later). Generated by `dirijor completion bash`.\n\
         # Install: dirijor completion bash > ~/.local/share/bash-completion/completions/dirijor\n\n\
         _dirijor() {\n  \
           local cur prev word key i skip=0 cmdpath='' subs='' flags='' pos='' candidates\n  \
           cur=\"${COMP_WORDS[COMP_CWORD]}\"\n  \
           prev=\"${COMP_WORDS[COMP_CWORD-1]}\"\n  \
           for (( i = 1; i < COMP_CWORD; i++ )); do\n    \
             word=\"${COMP_WORDS[i]}\"\n    \
             if [ \"$skip\" = 1 ]; then skip=0; continue; fi\n    \
             case \"$word\" in\n      ",
    );
    out.push_str(&format!(
        "{}) skip=1; continue ;;\n      -*) continue ;;\n    esac\n    \
         if [ -z \"$cmdpath\" ]; then key=\"$word\"; else key=\"$cmdpath $word\"; fi\n    \
         case \"$key\" in\n      {}) cmdpath=\"$key\" ;;\n      *) break ;;\n    esac\n  done\n  \
         case \"$cmdpath\" in\n",
        value_flag_case(model),
        path_case(model)
    ));
    let mut choices = String::new();
    for node in &model.nodes {
        let patterns = node
            .spellings
            .iter()
            .map(|spelling| quote(spelling))
            .collect::<Vec<_>>()
            .join("|");
        let subs = subcommands(node.command)
            .map(|(name, _)| name)
            .collect::<Vec<_>>()
            .join(" ");
        let flags = node
            .command
            .flags
            .iter()
            .flat_map(|flag| {
                std::iter::once(flag.long.to_owned()).chain(flag.short.map(|ch| format!("-{ch}")))
            })
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!(
            "    {patterns}) subs={}; flags={}; pos={} ;;\n",
            quote(&subs),
            quote(&flags),
            quote(positional_word(node.command.complete, node.command))
        ));
        for flag in node
            .command
            .flags
            .iter()
            .filter(|flag| !flag.choices.is_empty())
        {
            for spelling in &node.spellings {
                choices.push_str(&format!(
                    "    {}) COMPREPLY=( $(compgen -W {} -- \"$cur\") ); return ;;\n",
                    quote(&format!("{spelling} {}", flag.long)),
                    quote(&flag.choices.join(" "))
                ));
            }
        }
    }
    out.push_str("  esac\n  case \"$cmdpath $prev\" in\n");
    out.push_str(&choices);
    out.push_str(&format!(
        "  esac\n  \
         if [[ \"$cur\" == -* ]]; then\n    COMPREPLY=( $(compgen -W \"$flags\" -- \"$cur\") )\n    return\n  fi\n  \
         candidates=\"$subs\"\n  \
         case \"$pos\" in\n    \
           target) candidates=\"$candidates $(dirijor __complete targets 2>/dev/null | cut -f1)\" ;;\n    \
           target-key) candidates=\"$candidates {keys} $(dirijor __complete targets 2>/dev/null | cut -f1)\" ;;\n    \
           key) candidates=\"$candidates {keys}\" ;;\n    \
           edge) candidates=\"$candidates {edges}\" ;;\n    \
           shell) candidates=\"$candidates {shells}\" ;;\n    \
           path) COMPREPLY=( $(compgen -f -- \"$cur\") ); return ;;\n  \
         esac\n  \
         COMPREPLY=( $(compgen -W \"$candidates\" -- \"$cur\") )\n\
         }}\n\
         complete -F _dirijor dirijor\n",
        keys = spec::KEY_NAMES.join(" "),
        edges = spec::EDGES.join(" "),
        shells = spec::SHELLS.join(" "),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_completable_command_appears_in_every_script() {
        let model = Model::new();
        for shell in spec::SHELLS {
            let script = script(shell).unwrap();
            for path in model.command_paths() {
                assert!(script.contains(path), "{shell} lacks {path:?}");
            }
            assert!(script.contains("__complete targets"), "{shell}");
            assert!(!script.contains("mcp-stdio"), "{shell} lists internals");
        }
        assert!(script("powershell").is_none());
    }

    #[test]
    fn value_flags_are_skipped_by_the_path_scanner() {
        let model = Model::new();
        assert!(model.value_flags.iter().any(|flag| flag == "--cwd"));
        assert!(model.value_flags.iter().any(|flag| flag == "-c"));
        assert!(!model.value_flags.iter().any(|flag| flag == "--json"));
    }

    #[test]
    fn quoting_survives_apostrophes() {
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(fish_quote("it's"), "'it\\'s'");
    }
}
