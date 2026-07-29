# fish completion for asc(1) — AdminService.Cloud CLI.
#
# Install:  asc completion fish > /usr/share/fish/vendor_completions.d/asc.fish
#
# `asc __complete` answers with `value<TAB>description` lines — fish's native
# candidate format — optionally followed by a `:file` / `:dir` directive, which
# is where path completion takes over.

function __asc_complete
    set -l tokens (commandline --current-process --tokenize --cut-at-cursor)
    set -l current (commandline --current-token)
    # `"$current"` stays quoted: on `asc <Tab>` the token is empty, and an
    # unquoted empty expansion would drop the argument entirely — asc would
    # then complete the previous word.
    set -l out (asc __complete -- $tokens "$current" 2>/dev/null)

    for line in $out
        switch $line
            case ':dir'
                __fish_complete_directories $current
            case ':file'
                __fish_complete_path $current
            case ':*'
                # Unknown directive: ignore, never print it as a candidate.
            case '*'
                echo $line
        end
    end
end

# -f: no implicit filename completion — paths come from the directives above.
complete -c asc -f -a '(__asc_complete)'
