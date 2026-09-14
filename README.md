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
