# irontile

A Wayland tiling compositor with a split-container tree as its native model,
not a plugin on top of a floating one.

## Workspace

| Crate | What it is |
| --- | --- |
| `irontile-layout` | Tiling tree, desktops, and display arrangement. Pure integer geometry, no Wayland. |
| `irontile-comp` | The compositor. Owns every protocol and rendering concern. |
| `irontile-ipc` | Typed control-socket protocol, a thin envelope around the layout commands. |
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
placed in. `IRONTILE_TERMINAL` picks what the spawn binding launches; without
it, the first of a few common emulators found on `PATH` is used.

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
| `Super` + `Q` | Close the window |
| `Super` + `F` | Toggle fullscreen |
| `Super` `Shift` + `Space` | Toggle floating |
| `Super` + `V` / `B` | Split the container vertically / horizontally |
| `Super` + `T` | Flip the container's axis |
| `Super` + `O` | Equalize the container |
| `Super` `Shift` + `E` | Quit |

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
