# bash completion for irontilectl
#
# The word lists are written out rather than scraped from `irontilectl help`,
# because a completion that runs the program completes nothing on a machine
# where the program is broken, and because parsing help output is a promise
# about its formatting that nothing else makes. A test in the source tree
# checks these lists against the vocabulary the binary actually accepts, so
# drift fails the suite rather than the user.

_irontilectl() {
    local cur prev subcommands actions directions
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"

    subcommands="frame outputs workspaces windows layers layout watch version help"
    actions="focus move resize output send-to-output split equalize float \
fullscreen close workspace move-to-workspace spawn vt warp click press release \
reload quit"
    directions="left right up down"

    case "$prev" in
        focus|move|resize|output|send-to-output)
            COMPREPLY=($(compgen -W "$directions" -- "$cur"))
            return
            ;;
        split)
            COMPREPLY=($(compgen -W "horizontal vertical toggle" -- "$cur"))
            return
            ;;
        workspace)
            COMPREPLY=($(compgen -W "next prev" -- "$cur"))
            return
            ;;
        spawn)
            # Whatever can be run, which is what spawn takes.
            COMPREPLY=($(compgen -c -- "$cur"))
            return
            ;;
        click|press|release)
            COMPREPLY=($(compgen -W "1 2 3" -- "$cur"))
            return
            ;;
    esac

    # Only the first word can be a subcommand: everything after one is that
    # subcommand's business, and an action's arguments are handled above.
    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=($(compgen -W "$subcommands $actions" -- "$cur"))
    fi
}

complete -F _irontilectl irontilectl
