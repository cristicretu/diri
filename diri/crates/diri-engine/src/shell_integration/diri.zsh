# Diri shell integration for zsh: OSC 133 prompt and command marks.
#
#   A  prompt start   (in PS1, so a redrawn prompt is marked again)
#   B  input start    (in PS1, after the prompt text)
#   C  output start   (preexec)
#   D  command end    (precmd, with the command's exit status)
#
# Loaded from Diri's .zshenv shim before the user's other startup files. The
# hooks are only installed at the first prompt, after every startup file and
# prompt framework (oh-my-zsh, powerlevel10k, starship, ...) has run, and they
# go last so they see the PS1 those frameworks build.
[[ -o interactive ]] || builtin return 0
(( ${+_diri_command_running} )) && builtin return 0
builtin typeset -gi _diri_command_running=0

_diri_precmd() {
    builtin local -i _diri_status=$?
    builtin local _diri_percent=0
    [[ -o prompt_percent ]] && _diri_percent=1
    builtin emulate -L zsh
    if (( _diri_command_running )); then
        _diri_command_running=0
        (( ${+_p9k__iterm_cmd} )) ||
            builtin print -rn -- $'\e]133;D;'"$_diri_status"$'\a'
    fi
    # Frameworks rebuild PS1 in their own precmd; mark whatever it is now.
    if (( _diri_percent )) && [[ $PS1 != *$'\e]133;B\a'* ]]; then
        PS1=$'%{\e]133;A\a%}'"$PS1"$'%{\e]133;B\a%}'
    fi
    builtin return $_diri_status
}

_diri_preexec() {
    builtin emulate -L zsh
    _diri_command_running=1
    # powerlevel10k sends its own C and D once it draws the marks (below).
    (( ${+_p9k__iterm_cmd} )) || builtin print -rn -- $'\e]133;C\a'
}

_diri_deferred_init() {
    builtin local -i _diri_status=$?
    builtin emulate -L zsh
    precmd_functions=(${precmd_functions:#_diri_deferred_init})
    precmd_functions=(${precmd_functions:#_diri_precmd} _diri_precmd)
    preexec_functions=(${preexec_functions:#_diri_preexec} _diri_preexec)
    # zsh does not run hooks added while it runs them: mark this first prompt
    # here. A framework hook after this one may still rebuild it unmarked.
    _diri_precmd
}

# powerlevel10k moves its own precmd after every other hook and rebuilds
# PROMPT there, so it would drop the marks; it draws them itself when asked,
# the way VS Code's and kitty's integrations ask it to. The PS1 check above
# then finds its B mark and leaves the prompt alone.
(( ${+__p9k_force_term_shell_integration} )) || builtin typeset -gri __p9k_force_term_shell_integration=1

builtin typeset -ga precmd_functions preexec_functions
# The deferred init goes first so that, on the very first prompt, the moved
# _diri_precmd still runs after every other hook.
precmd_functions=(_diri_deferred_init ${precmd_functions[@]})
