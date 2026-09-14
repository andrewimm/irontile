# irontile

A Wayland tiling compositor with a split-container tree as its native model,
not a plugin on top of a floating one.

## Workspace

| Crate | What it is |
| --- | --- |
| `irontile-layout` | Tiling tree, desktops, and display arrangement. Pure integer geometry, no Wayland. |
| `irontile-comp` | The compositor. Owns every protocol and rendering concern. |
| `irontile-ipc` | Control-socket protocol, the shared action vocabulary, and `irontilectl`. |
| `irontile-ui` | Bar and launcher, as ordinary layer-shell clients. |
| `xtask` | Task runner. |

## Tasks

```
cargo xtask run      # build and launch the compositor
cargo xtask test     # rustfmt check, clippy with warnings denied, full test suite
```

## Running

```
cargo xtask run
```

This starts the nested backend: irontile opens as a window inside your current
compositor and prints the Wayland socket it bound. Clients pointed at that
socket are tiled inside it.

```
WAYLAND_DISPLAY=wayland-2 alacritty
```

`RUST_LOG=irontile=debug` logs every layout event and the cell each window is
placed in.

### Headless

```
cargo xtask run -- --headless 1920x1080,1280x1024
```

No renderer and as many displays as you ask for, driven entirely over the
control socket. This is how the integration tests run, and it is the only way
to exercise display arrangement, desktop transfer and hotplug without the
hardware to do it on.

### Control socket

Every binding is also a command:

```
irontilectl focus left
irontilectl workspace 3
irontilectl send-to-output right

irontilectl frame        # where every window is
irontilectl outputs      # displays and their arrangement
irontilectl workspaces   # desktops, and what is on them
irontilectl layout       # the whole engine state, as JSON
irontilectl watch        # stream events
```

`IRONTILE_SOCKET` targets a specific instance; otherwise the socket belonging to
`WAYLAND_DISPLAY` is used. Processes irontile spawns inherit both.

## Configuration

TOML at `$XDG_CONFIG_HOME/irontile/irontile.toml`, or wherever `--config`
points. There need not be one. `irontilectl reload` re-reads it, and a file that
fails to load leaves the running configuration in place rather than taking the
session down. `irontile --print-config` writes out the defaults.

```toml
[theme]
border_width = 2
border_focused = "#5c99d6"
border_unfocused = "#292b33"
background = "#121217"
inner_gap = 4
outer_gap = 4
resize_step = 40
terminal = "foot"        # omit to use the first one found on PATH

[layout]
smart_split = true       # split along the longer edge, rather than appending
default_axis = "horizontal"
reap_empty_workspaces = true
focus_follows_move = true

# A [binds] table replaces the defaults outright, so a binding can be removed.
[binds]
"Super+h" = "focus left"
"Super+Shift+h" = "move left"
"Super+Ctrl+h" = "resize left"
"Super+1" = "workspace 1"
"Super+Return" = "terminal"
"Super+Shift+e" = "quit"
```

Key names are xkb keysyms, so anything `xkbcli` prints works. Unknown settings
and unparseable bindings are errors naming the line, not silent no-ops.

### Bindings

All bindings are behind Super. Directions are `h`/`j`/`k`/`l` or the arrow keys.

| Binding | Action |
| --- | --- |
| `Super` + direction | Move focus, crossing to the next display at the edge |
| `Super` `Shift` + direction | Move the window, crossing displays at the edge |
| `Super` `Ctrl` + direction | Resize |
| `Super` `Alt` + direction | Move focus to another display |
| `Super` `Shift` `Alt` + direction | Send this desktop to another display |
| `Super` + `1`–`9`, `0` | Show desktop 1–10, creating it on first use |
| `Super` `Shift` + `1`–`9`, `0` | Send the window to that desktop |
| `Super` + `Return` | Spawn a terminal |
| `Super` `Shift` + `C` | Reload the configuration |
| `Super` + `Q` | Close the window |
| `Super` + `F` | Toggle fullscreen |
| `Super` `Shift` + `Space` | Toggle floating |
| `Super` + `V` / `B` | Split the container vertically / horizontally |
| `Super` + `T` | Flip the container's axis |
| `Super` + `O` | Equalize the container |
| `Super` `Shift` + `E` | Quit |
| `Super` + right-drag | Resize; the edges nearest where the drag started follow the pointer |

## Protocols

| Protocol | Notes |
| --- | --- |
| `wl_compositor`, `wl_shm`, `wl_seat`, `wl_output`, `xdg_output` | |
| `xdg_shell` | Toplevels and popups, with grabs, so menus dismiss |
| `xdg-decoration` | Every request is answered server-side; clients never draw their own titlebars |
| `wlr-layer-shell` | Bars and panels; exclusive zones shrink the work area windows tile into |
| `wl_data_device`, `primary-selection` | Clipboard and middle-click paste |
| `cursor-shape` | Clients name a cursor rather than supplying a buffer |

Not yet implemented: `linux-dmabuf`, so clients render into shared memory
rather than handing over GPU buffers; XWayland; `viewporter` and
`fractional-scale`; `pointer-constraints` and `relative-pointer`.

## Design notes

**The layout engine is data in, data out.** `dispatch` applies a `Command` and
reports `Event`s; `frame` returns a complete snapshot of where every window
goes. Nothing but plain serializable data crosses that line — no handles, no
callbacks, no borrowed state — so the engine can move behind a serialization
boundary later without the compositor changing.

**Everything is integers.** Sizes come from integer weights rather than
fractional ratios, so dividing a rectangle is exact arithmetic. That is what
makes "the produced rectangles exactly tile the input" a property that holds
rather than one that holds up to rounding, and it keeps the state bit-for-bit
reproducible across a host/guest boundary.

**Displays are rectangles in one coordinate space.** The arrangement of monitors
is nothing more than where their rectangles sit, so moving focus or a window
between displays is the same geometry as moving it within one.

**Desktops are unbounded and discrete.** Each display shows exactly one; each
desktop remembers which display it prefers, so unplugging and replugging a
monitor restores what was on it. Sending a desktop to a display takes that
display over, and the display it came from gets a fresh desktop — so moving
several desktops onto one monitor in a row does what you meant each time.

**Desktops are addressed by id, numbered by convention.** The layout engine has
no notion of "workspace 4"; the compositor names desktops `"1"`, `"2"` and so on
and resolves a number to an id on first use. Fixed numeric slots are a special
case of that, not the other way round.

**One vocabulary for bindings and the socket.** A key binding, a line in the
config file and an `irontilectl` invocation all name the same `Action` and run
the same code, so they cannot drift apart. Precise, addressed operations — this
window, that desktop — go over the socket as the layout engine's own commands
instead.

**The compositor is testable because the socket exists.** `Query::Layout`
returns the entire engine state, which deserializes into a real `Layout`, so a
test can assert on the tree or call `validate()` on it without the compositor
growing a reporting API of its own. Combined with the headless backend, that is
how display arrangement and hotplug are covered end to end.

**Decoration is not the client's decision.** The compositor draws a border
rectangle and nothing else, so every `xdg-decoration` request is answered
server-side whatever it asked for. Without the protocol advertised at all,
toolkits fall back to drawing their own titlebars and shadows, which is worse
than either choice made deliberately.

**The work area is what layer-shell leaves behind.** A bar reserving thirty
pixels at the top of a display shrinks that display's work area, and the layout
engine tiles into what is left. The engine never learns that layer-shell exists;
it is handed a rectangle.
