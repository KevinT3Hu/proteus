# Proteus shell integration hooks for zsh (OSC 133 marks, OSC 7 cwd).

[[ -o interactive && -z "${__proteus_loaded-}" ]] || builtin return 0
typeset -g __proteus_loaded=1 __proteus_executing=

__proteus_urlencode() {
    emulate -L zsh
    setopt localoptions nomultibyte
    local s=$1 c i
    REPLY=
    for (( i = 1; i <= ${#s}; i++ )); do
        c=${s[i]}
        if [[ $c == [A-Za-z0-9._~/-] ]]; then
            REPLY+=$c
        else
            builtin printf -v c '%%%02X' "'$c"
            REPLY+=$c
        fi
    done
}

__proteus_report_cwd() {
    __proteus_urlencode "$PWD"
    builtin print -rn -- $'\e]7;file://'"${HOST}${REPLY}"$'\a'
}

__proteus_precmd() {
    local ret=$?
    if [[ -n $__proteus_executing ]]; then
        builtin printf '\e]133;D;%s\a' "$ret"
        __proteus_executing=
    fi
    __proteus_report_cwd
    # Wrap the prompt each time: themes may rebuild PS1 in their own precmd.
    if [[ $PS1 != *$'\e]133;A'* ]]; then
        PS1=$'%{\e]133;A\a%}'"$PS1"$'%{\e]133;B\a%}'
    fi
}

__proteus_preexec() {
    __proteus_executing=1
    __proteus_urlencode "$1"
    builtin printf '\e]133;C;cmdline_url=%s\a' "$REPLY"
}

# Register on the first prompt so our precmd runs after every hook added by .zshrc.
__proteus_deferred_init() {
    builtin autoload -Uz add-zsh-hook
    add-zsh-hook -d precmd __proteus_deferred_init
    add-zsh-hook precmd __proteus_precmd
    add-zsh-hook preexec __proteus_preexec
    __proteus_precmd
}
builtin autoload -Uz add-zsh-hook
add-zsh-hook precmd __proteus_deferred_init
