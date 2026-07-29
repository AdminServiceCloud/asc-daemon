# bash completion for asc(1) — AdminService.Cloud CLI.
#
# Install:  asc completion bash > /usr/share/bash-completion/completions/asc
#
# The script holds no command list of its own: every candidate comes from
# `asc __complete`, so a new command or a newly installed app is completable
# without regenerating anything. Whatever asc cannot answer falls through to
# the shell's own filename completion (`-o default`), which is what keeps
# `asc backup restore /et<Tab>` behaving like plain `ls /et<Tab>`.

_asc() {
    local cur words cword out line value
    # bash splits COMP_WORDS on COMP_WORDBREAKS, which chops `--flag=value`
    # and `user@host:path` apart; bash-completion's helper puts them back
    # together. It is not guaranteed to be loaded (this file may be sourced
    # by hand), hence the fallback.
    if declare -F _get_comp_words_by_ref >/dev/null 2>&1; then
        _get_comp_words_by_ref -n "=:" cur words cword
    else
        words=("${COMP_WORDS[@]}")
        cword=$COMP_CWORD
        cur=${COMP_WORDS[COMP_CWORD]}
    fi

    COMPREPLY=()
    out=$(asc __complete -- "${words[@]:0:cword}" "$cur" 2>/dev/null) || return 0

    while IFS= read -r line; do
        [ -n "$line" ] || continue
        case $line in
            # Directives: asc asks the shell to complete paths itself.
            :dir)
                compopt -o dirnames 2>/dev/null
                return 0
                ;;
            :file)
                compopt -o default 2>/dev/null
                return 0
                ;;
            :*) continue ;;
        esac
        # `value<TAB>description` — bash shows values only.
        value=${line%%$'\t'*}
        COMPREPLY+=("$value")
    done <<< "$out"

    # A real candidate list is the whole answer; without this bash would add
    # filenames next to app ids (the `-o default` registered below).
    if [ ${#COMPREPLY[@]} -gt 0 ]; then
        compopt +o default 2>/dev/null
    fi
    return 0
}

complete -o default -o bashdefault -F _asc asc
