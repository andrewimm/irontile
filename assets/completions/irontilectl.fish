# fish completion for irontilectl
#
# The word lists are written out rather than scraped from `irontilectl help`,
# because a completion that runs the program completes nothing on a machine
# where the program is broken, and because parsing help output is a promise
# about its formatting that nothing else makes. A test in the source tree
# checks these lists against the vocabulary the binary actually accepts, so
# drift fails the suite rather than the user.

# Nothing this takes is a path, and the directory listing offered for
# `irontilectl fo<TAB>` is worse than nothing at all.
complete -c irontilectl -f

# Every first word, so that none of them is offered a second time. Expanded
# where it is used, which is now: a completion file is read once.
set -l words frame outputs workspaces windows layers layout watch version help \
    focus move resize output send-to-output split equalize float fullscreen \
    close workspace move-to-workspace spawn vt warp click press release reload \
    quit
set -l first "not __fish_seen_subcommand_from $words"

complete -c irontilectl -n $first -a frame -d "Where every window is right now"
complete -c irontilectl -n $first -a outputs -d "Connected displays and their arrangement"
complete -c irontilectl -n $first -a workspaces -d "Every desktop, and what is on it"
complete -c irontilectl -n $first -a windows -d "Every window, with its title and application id"
complete -c irontilectl -n $first -a layers -d "Panels and overlays, and what they reserve"
complete -c irontilectl -n $first -a layout -d "The whole layout engine state, as JSON"
complete -c irontilectl -n $first -a watch -d "Stream events until interrupted"
complete -c irontilectl -n $first -a version -d "Show the version and exit"
complete -c irontilectl -n $first -a help -d "Show a usage message"

complete -c irontilectl -n $first -a focus -d "Move the keyboard focus"
complete -c irontilectl -n $first -a move -d "Move the focused window"
complete -c irontilectl -n $first -a resize -d "Grow or shrink the focused window"
complete -c irontilectl -n $first -a output -d "Focus the display in that direction"
complete -c irontilectl -n $first -a send-to-output -d "Send the focused desktop to that display"
complete -c irontilectl -n $first -a split -d "How the next window splits"
complete -c irontilectl -n $first -a equalize -d "Give every sibling the same share"
complete -c irontilectl -n $first -a float -d "Take the window out of the tree, or put it back"
complete -c irontilectl -n $first -a fullscreen -d "Fill the display, or stop"
complete -c irontilectl -n $first -a close -d "Ask the focused window to close"
complete -c irontilectl -n $first -a workspace -d "Show a desktop"
complete -c irontilectl -n $first -a move-to-workspace -d "Send the focused window to a desktop"
complete -c irontilectl -n $first -a spawn -d "Run a program"
complete -c irontilectl -n $first -a vt -d "Switch to a virtual terminal"
complete -c irontilectl -n $first -a warp -d "Put the pointer somewhere"
complete -c irontilectl -n $first -a click -d "Synthesize a click"
complete -c irontilectl -n $first -a press -d "Hold a pointer button down"
complete -c irontilectl -n $first -a release -d "Let a pointer button go"
complete -c irontilectl -n $first -a reload -d "Read the configuration file again"
complete -c irontilectl -n $first -a quit -d "End the session"

complete -c irontilectl -n "__fish_seen_subcommand_from focus move resize output send-to-output" -a "left right up down"
complete -c irontilectl -n "__fish_seen_subcommand_from split" -a "horizontal vertical toggle"
complete -c irontilectl -n "__fish_seen_subcommand_from workspace" -a "next prev"
complete -c irontilectl -n "__fish_seen_subcommand_from click press release" -a "1 2 3"
# spawn takes a program, so offer what can be run.
complete -c irontilectl -n "__fish_seen_subcommand_from spawn" -a "(__fish_complete_command)"
