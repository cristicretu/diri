# Diri shell integration for bash: OSC 133 prompt and command marks.
#
#   A  prompt start   (in PS1, so a redrawn prompt is marked again)
#   B  input start    (in PS1, after the prompt text)
#   C  output start   (bash-preexec's preexec, else a chained DEBUG trap)
#   D  command end    (at the next prompt, with the command's exit status)
#
# Diri starts the user's login shell unchanged (`bash -l`) with
# PROMPT_COMMAND set to source this file, so it runs at the first prompt,
# after /etc/profile, ~/.bash_profile and ~/.bashrc and every prompt framework
# they load. A startup file that replaces PROMPT_COMMAND outright simply
# leaves the shell without marks.
#
# Everything here keeps $? and $_ as the user's own prompt code would see
# them, survives `set -u`, and works in bash 3.2 (macOS /bin/bash).

if [[ $- == *i* && -z "${__diri_loaded-}" ]]; then
__diri_loaded=1
# The bootstrap was exported to bash through the environment; it must not
# reach the programs this shell runs.
builtin export -n PROMPT_COMMAND

# Whether a command line ran since the last prompt (C was sent).
__diri_ran=
# Set once the prompt is drawn; the next simple command is the user's.
__diri_armed=

# Returns the status it was called with, so `__diri_keep` in place of the
# bootstrap leaves $? untouched for the PROMPT_COMMAND code after it.
__diri_keep() { builtin return $?; }

__diri_precmd() {
    builtin local __diri_status=$?
    __diri_armed=
    if [[ "$__diri_ran" == 1 ]]; then
        __diri_ran=
        builtin printf '\033]133;D;%s\007' "$__diri_status"
    fi
    builtin return $__diri_status
}

__diri_prompt_end() {
    builtin local __diri_status=$?
    # Frameworks rebuild PS1 in their own prompt command; mark whatever it is now.
    if [[ "$PS1" != *'133;B'* ]]; then
        PS1='\[\033]133;A\007\]'"$PS1"'\[\033]133;B\007\]'
    fi
    __diri_armed=1
    builtin return $__diri_status
}

__diri_preexec() {
    __diri_armed=
    __diri_ran=1
    builtin printf '\033]133;C\007'
}

# DEBUG trap body: the first simple command after the prompt is the user's
# command line. PROMPT_COMMAND itself starts with __diri_precmd, and this
# file's own commands all name a __diri_ variable or function.
__diri_debug() {
    __diri_debug_status=$1
    __diri_debug_last=$2
    [[ -n "$__diri_armed" && -z "${COMP_LINE-}" ]] || builtin return 0
    case "$BASH_COMMAND" in
        *__diri_*) builtin return 0 ;;
    esac
    # At the first prompt the rest of the startup PROMPT_COMMAND still runs
    # after this file installed the trap.
    if [[ "$__diri_armed" == boot ]]; then
        [[ "$__diri_boot_prompt_command" != *"$BASH_COMMAND"* ]] || builtin return 0
        builtin unset __diri_boot_prompt_command
    fi
    __diri_preexec
    builtin return 0
}

# Sets $? to $1, and $_ to its last argument.
__diri_restore() { builtin return "$1"; }

__diri_install() {
    # Exactly what the Engine put in PROMPT_COMMAND.
    builtin local boot=". '${BASH_SOURCE[0]}'; builtin eval \"\${__diri_trap_install-}\""
    __diri_boot_prompt_command="${PROMPT_COMMAND[*]}"
    # __diri_precmd goes first, to see the command's status, and
    # __diri_prompt_end after every other prompt hook, which may rebuild PS1
    # (starship, a hand-written prompt function).
    builtin local first=__diri_precmd
    if [[ -n "${bash_preexec_imported-}${__bp_imported-}" ]]; then
        # bash-preexec runs precmd_functions first in PROMPT_COMMAND and keeps
        # it wrapped in its own pair of hooks, so a plain hook at the end still
        # runs after them. It owns the DEBUG trap (or PS0); join it.
        first=
        precmd_functions=(__diri_precmd ${precmd_functions[@]+"${precmd_functions[@]}"})
        preexec_functions+=(__diri_preexec)
    else
        __diri_trap=1
    fi
    if [[ "$(builtin declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
        PROMPT_COMMAND=($first "${PROMPT_COMMAND[@]//"$boot"/__diri_keep}" __diri_prompt_end)
    else
        PROMPT_COMMAND="${first:+$first$'\n'}${PROMPT_COMMAND//"$boot"/__diri_keep}"$'\n__diri_prompt_end'
    fi
}
__diri_trap=
__diri_install
builtin unset -f __diri_install
if [[ -n "$__diri_trap" ]]; then
    # Run before any DEBUG trap already set (starship's, a user's), handing it
    # the $? and $_ it would have seen. The bootstrap evals this right after
    # sourcing the file: `.` puts back a DEBUG trap that existed before it.
    builtin read -r -d '' __diri_trap_install <<'__DIRI_TRAP__'
__diri_trap="$(builtin trap -p DEBUG)"
if [[ -z "$__diri_trap" ]]; then
    builtin trap '__diri_debug "$?" "$_"; : "$__diri_debug_last"' DEBUG
else
    __diri_trap="${__diri_trap#trap -- }"
    __diri_trap="${__diri_trap% DEBUG}"
    builtin eval "__diri_trap=$__diri_trap"
    builtin trap '__diri_debug "$?" "$_"; __diri_restore "$__diri_debug_status" "$__diri_debug_last"; '"$__diri_trap" DEBUG
fi
builtin unset __diri_trap __diri_trap_install
__DIRI_TRAP__
fi
# The first prompt is drawn right after this; mark it too. `boot` makes the
# DEBUG trap skip the rest of the startup PROMPT_COMMAND (see __diri_debug).
__diri_first_prompt() {
    __diri_prompt_end
    __diri_armed=boot
}
__diri_first_prompt
fi
