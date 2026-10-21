# Proteus shell integration for zsh: loaded because Proteus points ZDOTDIR here.
# Restore the user's ZDOTDIR, run their .zshenv, then load the hooks.

if [[ -n "${PROTEUS_ZSH_ZDOTDIR+X}" ]]; then
    builtin export ZDOTDIR="$PROTEUS_ZSH_ZDOTDIR"
    builtin unset PROTEUS_ZSH_ZDOTDIR
else
    builtin unset ZDOTDIR
fi

{
    builtin typeset __proteus_f="${ZDOTDIR-$HOME}/.zshenv"
    [[ -r "$__proteus_f" ]] && builtin source -- "$__proteus_f"
} always {
    if [[ -o interactive && -n "${PROTEUS_SHELL_INTEGRATION_DIR-}" ]]; then
        builtin source -- "$PROTEUS_SHELL_INTEGRATION_DIR/zsh/proteus.zsh"
    fi
    builtin unset __proteus_f
}
