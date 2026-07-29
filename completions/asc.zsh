#compdef asc
# zsh completion for asc(1) — AdminService.Cloud CLI.
#
# Install:  asc completion zsh > /usr/share/zsh/vendor-completions/_asc
#
# Like the bash script, this one knows no commands: `asc __complete` answers
# with `value<TAB>description` lines, optionally followed by a `:file` / `:dir`
# directive asking zsh to complete paths itself.

_asc() {
    local -a lines candidates
    local line directive value description

    # The current word is passed quoted on purpose: it is empty on `asc <Tab>`,
    # and an unquoted empty expansion would vanish, leaving asc to complete the
    # previous word instead.
    lines=(${(f)"$(asc __complete -- ${words[1,CURRENT-1]} "${words[CURRENT]}" 2>/dev/null)"})

    for line in $lines; do
        [[ -n $line ]] || continue
        if [[ $line == :* ]]; then
            directive=$line
            continue
        fi
        value=${line%%$'\t'*}
        description=${line#*$'\t'}
        if [[ $description == $line ]]; then
            candidates+=("$value")
        else
            candidates+=("$value:$description")
        fi
    done

    if (( ${#candidates} )); then
        _describe -t asc-candidates 'asc' candidates
        return
    fi

    case $directive in
        :dir) _files -/ ;;
        :file) _files ;;
        *) return 1 ;;
    esac
}

_asc "$@"
