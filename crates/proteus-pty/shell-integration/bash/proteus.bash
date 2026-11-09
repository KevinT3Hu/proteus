# Proteus shell integration for bash.
#
# Injected via `bash --posix` + ENV: in POSIX mode an interactive bash reads
# only $ENV, so this file switches POSIX mode back off and sources the usual
# startup files itself before installing the hooks.

if [[ -n "${PROTEUS_BASH_INJECT-}" ]]; then
    builtin set +o posix
    builtin unset ENV PROTEUS_BASH_INJECT
    if [[ -n "${PROTEUS_BASH_ENV+X}" ]]; then
        builtin export ENV="$PROTEUS_BASH_ENV"
        builtin unset PROTEUS_BASH_ENV
    fi
    if builtin shopt -q login_shell; then
        [[ -r /etc/profile ]] && builtin source /etc/profile
        for __proteus_f in ~/.bash_profile ~/.bash_login ~/.profile; do
            if [[ -r "$__proteus_f" ]]; then
                builtin source "$__proteus_f"
                break
            fi
        done
    else
        [[ -r /etc/bash.bashrc ]] && builtin source /etc/bash.bashrc
        [[ -r ~/.bashrc ]] && builtin source ~/.bashrc
    fi
    builtin unset __proteus_f
fi


if [[ "$-" == *i* && -z "${__proteus_loaded-}" && "${BASH_VERSINFO[0]:-0}${BASH_VERSINFO[1]:-0}" -ge 44 ]]; then
    __proteus_loaded=1
    __proteus_executing=""


    # Percent-encode $1 into __proteus_url (bytewise, no subshell).
    __proteus_urlencode() {
        local LC_ALL=C s="$1" i c
        __proteus_url=""
        for ((i = 0; i < ${#s}; i++)); do
            c="${s:i:1}"
            case "$c" in
                [a-zA-Z0-9.~_/-]) __proteus_url+="$c" ;;
                *) builtin printf -v c '%%%02X' "'$c"; __proteus_url+="$c" ;;
            esac
        done
    }

    # Runs first in PROMPT_COMMAND: report the previous command's status and cwd.
    __proteus_precmd() {
        local ret="$?"
        if [[ -n "$__proteus_executing" ]]; then
            builtin printf '\e]133;D;%s\a' "$ret"
            __proteus_executing=""
        fi
        __proteus_urlencode "$HOSTNAME"
        local host="$__proteus_url"
        __proteus_urlencode "$PWD"
        builtin printf '\e]7;file://%s%s\a' "$host" "$__proteus_url"
        return "$ret"
    }


    # PS0 is expanded in the parent shell right before a command runs (also for
    # subshells and pipelines, unlike a DEBUG trap). The array subscript sets the
    # "executing" flag as a side effect of the expansion.
    __proteus_ps0='${__proteus_unused[__proteus_executing=1]-}\e]133;C\a'

    # Runs last in PROMPT_COMMAND: (re)apply marks; prompt frameworks may rewrite PS1/PS0.
    __proteus_ps1() {
        if [[ "${PS1-}" != *'133;A'* ]]; then
            PS1='\[\e]133;A\a\]'"${PS1-}"'\[\e]133;B\a\]'
        fi
        # The PS0 mark relies on parameter expansion in prompts (promptvars);
        # without it the text would be printed literally before each command.
        if builtin shopt -q promptvars; then
            if [[ "${PS0-}" != *'133;C'* ]]; then
                PS0="$__proteus_ps0${PS0-}"
            fi
        elif [[ "${PS0-}" == "$__proteus_ps0"* ]]; then
            PS0="${PS0#"$__proteus_ps0"}"
        fi
    }

    if [[ -n "${bash_preexec_imported:-}${__bp_imported:-}" ]]; then
        # bash-preexec owns PROMPT_COMMAND; it preserves $? for each precmd function.
        precmd_functions=(__proteus_precmd "${precmd_functions[@]}" __proteus_ps1)
    elif [[ "$(builtin declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
        PROMPT_COMMAND=(__proteus_precmd "${PROMPT_COMMAND[@]}" __proteus_ps1)
    else
        __proteus_pc="${PROMPT_COMMAND-}"
        while [[ "$__proteus_pc" == *[$'; \n\t'] ]]; do __proteus_pc="${__proteus_pc%?}"; done
        PROMPT_COMMAND="__proteus_precmd${__proteus_pc:+;$__proteus_pc};__proteus_ps1"
        builtin unset __proteus_pc
    fi
fi
