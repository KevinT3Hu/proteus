# Proteus shell integration for fish: found because Proteus prepends its
# directory to XDG_DATA_DIRS. Remove it again so child processes see the
# user's original value.
if set -q PROTEUS_FISH_XDG_DATA_DIR
    set -l dirs (string split : -- "$XDG_DATA_DIRS")
    set -l keep
    for d in $dirs
        test "$d" = "$PROTEUS_FISH_XDG_DATA_DIR"; or set -a keep $d
    end
    if test (count $keep) -gt 0
        set -gx XDG_DATA_DIRS (string join : -- $keep)
    else
        set -e XDG_DATA_DIRS
    end
    set -e PROTEUS_FISH_XDG_DATA_DIR
end

status is-interactive; or exit 0
set -q __proteus_loaded; and exit 0
set -g __proteus_loaded 1

function __proteus_report_cwd --on-variable PWD
    printf '\e]7;file://%s%s\a' (string escape --style=url -- $hostname) (string escape --style=url -- $PWD)
end

function __proteus_preexec --on-event fish_preexec
    set -g __proteus_executing 1
    printf '\e]133;C;cmdline_url=%s\a' (string escape --style=url -- "$argv[1]")
end

function __proteus_postexec --on-event fish_postexec
    set -l ret $status
    if set -q __proteus_executing
        set -e __proteus_executing
        printf '\e]133;D;%s\a' $ret
    end
end

function __proteus_return
    return $argv[1]
end

# Wrap fish_prompt on the first prompt, after config.fish had a chance to define it.
function __proteus_init_prompt --on-event fish_prompt
    functions -e __proteus_init_prompt
    if functions -q fish_prompt
        functions -c fish_prompt __proteus_original_fish_prompt
        function fish_prompt
            set -l s $status
            printf '\e]133;A\a'
            # Keep $status for prompts that display it.
            __proteus_return $s
            __proteus_original_fish_prompt
            printf '\e]133;B\a'
        end
    end
    __proteus_report_cwd
end
