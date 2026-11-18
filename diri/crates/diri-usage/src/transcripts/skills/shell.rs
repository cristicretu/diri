use serde_json::Value;

pub(super) fn skill_from_path(path: &str) -> Option<String> {
    let mut parts = path.trim_end().rsplit(['/', '\\']);
    if parts.next()? != "SKILL.md" {
        return None;
    }
    let folder = parts.next()?;
    (!matches!(folder, "" | "." | "..")).then(|| folder.to_owned())
}

/// Code-mode transcripts may wrap a shell call in JavaScript. Accept a direct
/// call with a literal cmd; never search arbitrary JS strings for shell words.
pub(super) fn wrapped_literal(input: &str) -> Option<(&str, String)> {
    let input = input.trim().trim_end_matches(';').trim();
    let input = input
        .strip_prefix("text(")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(input);
    let input = input
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(").output"))
        .unwrap_or(input);
    let (tool, args) = input.strip_prefix("await tools.")?.split_once('(')?;
    let field = match tool {
        "exec_command" => "cmd",
        "mcp__dirijor__get_skill" => "name",
        _ => return None,
    };
    let body = args
        .strip_suffix(')')?
        .trim()
        .strip_prefix('{')?
        .strip_suffix('}')?
        .trim();
    let (key, value) = body.split_once(':')?;
    if key.trim().trim_matches(['\'', '"']) != field {
        return None;
    }
    let value = value.trim_start();
    let (literal, tail) = if let Some(quoted) = value.strip_prefix('\'') {
        let (literal, tail) = quoted.split_once('\'')?;
        // Escaped JS strings need an interpreter. Only accept simple literals here.
        if literal.contains('\\') {
            return None;
        }
        (literal.to_owned(), tail)
    } else {
        let mut stream = serde_json::Deserializer::from_str(value).into_iter::<String>();
        let literal = stream.next()?.ok()?;
        (literal, &value[stream.byte_offset()..])
    };
    let tail = tail.trim();
    (tail.is_empty() || tail.starts_with(',')).then_some((tool, literal))
}

pub(super) fn shell_reads(command: &Value) -> Vec<String> {
    if let Some(command) = command.as_str() {
        return skills_read_by(command);
    }
    let Some(args) = command
        .as_array()
        .and_then(|args| args.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
    else {
        return Vec::new();
    };
    match args.as_slice() {
        [program, option, script, ..]
            if matches!(
                program.rsplit(['/', '\\']).next(),
                Some("sh" | "bash" | "zsh" | "fish")
            ) && matches!(*option, "-c" | "-lc") =>
        {
            skills_read_by(script)
        }
        [program, option, script]
            if matches!(
                program.to_ascii_lowercase().as_str(),
                "pwsh" | "powershell" | "powershell.exe" | "pwsh.exe"
            ) && option.eq_ignore_ascii_case("-command") =>
        {
            skills_read_by(script)
        }
        _ => read_operands(&args),
    }
}

fn read_operands(words: &[&str]) -> Vec<String> {
    let Some((program, args)) = words.split_first() else {
        return Vec::new();
    };
    let verb = program.rsplit(['/', '\\']).next().unwrap_or_default();
    let paths = match verb {
        "cat" | "head" | "tail" | "nl" | "bat" | "less" | "more" => args,
        "sed" | "awk" => {
            // The first non-option argument is a program, not a path.
            if args
                .iter()
                .any(|arg| arg.starts_with("-i") || arg.starts_with("--in-place"))
            {
                return Vec::new();
            }
            let Some(script) = args.iter().position(|arg| !arg.starts_with('-')) else {
                return Vec::new();
            };
            // Only recognize printing programs used to inspect a file.
            let program = args[script];
            if !(verb == "sed"
                && program.ends_with('p')
                && program[..program.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, ',' | '$'))
                || verb == "awk"
                    && matches!(program.trim(), "{print}" | "{ print }" | "{print $0}"))
            {
                return Vec::new();
            }
            &args[script + 1..]
        }
        cmd if cmd.eq_ignore_ascii_case("Get-Content") => args,
        _ => return Vec::new(),
    };
    paths
        .iter()
        .filter(|path| !path.starts_with('-'))
        .filter_map(|path| skill_from_path(path))
        .collect()
}

/// Tokenize literal shell arguments and command boundaries. Dynamic shell
/// constructs are ambiguous in transcripts and are skipped.
pub(super) fn skills_read_by(command: &str) -> Vec<String> {
    let mut chars = command.chars().peekable();
    let mut quote = None;
    let mut word = String::new();
    let mut words = Vec::new();
    let mut names = Vec::new();
    while let Some(c) = chars.next() {
        if quote == Some(c) {
            quote = None;
        } else if let Some(q) = quote {
            if q == '"' && (c == '`' || c == '$' && chars.peek() == Some(&'(')) {
                return Vec::new();
            }
            if q == '"' && c == '\\' && chars.peek().is_some_and(|c| matches!(c, '"' | '\\')) {
                word.push(chars.next().unwrap());
            } else {
                word.push(c);
            }
        } else {
            match c {
                '\'' | '"' => quote = Some(c),
                '#' if word.is_empty() => break,
                '>' if word == "2" => {
                    word.clear();
                    if chars.peek() == Some(&'&') {
                        chars.next();
                    }
                    let dest: String = std::iter::from_fn(|| {
                        chars.next_if(|c| !c.is_whitespace() && !matches!(c, ';' | '|' | '&'))
                    })
                    .collect();
                    if !matches!(dest.as_str(), "1" | "/dev/null") {
                        return Vec::new();
                    }
                }
                '>' | '<' | '(' | ')' | '`' => return Vec::new(),
                c if c.is_whitespace() || matches!(c, ';' | '|' | '&') => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    if matches!(c, ';' | '|' | '&' | '\n') {
                        names.extend(read_operands(
                            &words.iter().map(String::as_str).collect::<Vec<_>>(),
                        ));
                        words.clear();
                    }
                }
                '\\' if chars.peek().is_some_and(|next| {
                    next.is_whitespace() || matches!(next, '\'' | '"' | ';' | '&' | '|')
                }) =>
                {
                    word.push(chars.next().unwrap())
                }
                _ => word.push(c),
            }
        }
    }
    if quote.is_some() {
        return Vec::new();
    }
    if !word.is_empty() {
        words.push(word);
    }
    names.extend(read_operands(
        &words.iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    names
}
