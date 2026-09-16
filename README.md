# irontile

A Wayland tiling compositor with a split-container tree as its native model,
not a plugin on top of a floating one.

<img width="2256" height="1504" alt="2026-09-14_180519" src="https://github.com/user-attachments/assets/f554458f-f9a5-42f4-9deb-2d639aecdabd" />

## Installing

Packages for each release are on the
[releases page](https://github.com/andrewimm/irontile/releases): an Arch
package, an rpm and a deb. Each is built inside the distribution it installs
on, and installed again in a clean container of that distribution before it is
published, so a package that cannot be installed never reaches the page.

```
# Arch
curl -LO https://github.com/andrewimm/irontile/releases/download/v0.2.6/irontile-0.2.6-1-x86_64.pkg.tar.zst
sudo pacman -U irontile-0.2.6-1-x86_64.pkg.tar.zst

# Fedora
sudo dnf install ./irontile-0.2.6-1.x86_64.rpm

# Debian, Ubuntu
sudo apt install ./irontile_0.2.6-1_amd64.deb
```

Then pick irontile at your login manager, or run `start-irontile` from a
virtual terminal. Both are covered below.

The packages attached to a release carry build provenance, so a downloaded one
can be tied back to the commit and workflow run that produced it:

```
gh attestation verify irontile-0.2.6-1-x86_64.pkg.tar.zst --repo andrewimm/irontile
```

Nothing here is signed with a GPG key, deliberately. A key kept in the
repository's secrets would be held by whoever holds the account that publishes
the releases, which is the same account somebody would have to take to publish a
bad one; the certificate behind the attestation lives for minutes and there is
nothing to leak. This is also why `pacman -U` on a release URL asks for a
`.sig` and does not find one: download the package first and install the file.

Five binaries, a desktop entry and a PAM service:

| File | Where it goes |
| --- | --- |
| `irontile` | `/usr/bin` |
| `irontile-bar` | `/usr/bin` |
| `irontile-lock` | `/usr/bin` |
| `irontilectl` | `/usr/bin` |
| `start-irontile` | `/usr/bin` |
| `assets/irontile.desktop` | `/usr/share/wayland-sessions/` |
| `assets/pam/<distribution>` | `/etc/pam.d/irontile-lock` |

The desktop entry is the whole of what makes irontile selectable in GDM, SDDM,
greetd or anything else that reads the directory. Without it the binaries are
installed and nothing offers to start them.

The PAM file decides what unlocking the screen requires, which is the
administrator's to change rather than this program's to assume. There is one per
distribution because the stack a graphical locker should defer to is named
differently on each, and the packages install the right one. Without the file
nothing authenticates -- which is the safe way round.

`start-irontile` is a supervisor, not a launcher. It starts the compositor,
waits for it, and starts it again if it died:

```
start-irontile              # same as: start-irontile -- --session
start-irontile -- --config /tmp/other.toml
start-irontile --path ./target/release/irontile
```

A compositor is the one process in a session nothing else can stand in for, so
losing it means losing every window *and* the display server, which from the
chair looks like a hang. Restarting turns that into a blink. The windows are
still gone -- this is a restart, not a resurrection.

Exiting cleanly ends the session, and so does being asked to stop: a login
manager ending the session and `Ctrl+C` at a terminal both arrive as a signal,
which is passed on to the compositor and then honoured rather than read as a
crash. Anything else is a crash. Five of those inside a minute and the
supervisor stops, because a compositor that dies during startup -- a GPU it
cannot drive, a display it cannot modeset -- would otherwise be restarted
forever, with the seat flickering and no way back to a terminal.

Without a login manager, a TTY works the same way:

```
exec start-irontile
```

Each binary answers `--version` with the commit it was built from, because a
version number alone cannot tell a release from a package built out of a
working tree that happens to carry the same number:

```
$ irontile --version
irontile 0.2.6 (e8e57c26)
```

A `-dirty` on the end means it was built from a tree with uncommitted changes
in it, and so from something nobody else can reproduce. The commit is missing
where there was no git to ask, which is better than inventing one.

`irontilectl version` answers a different question -- what is *running*:

```
$ irontilectl version
irontilectl 0.2.6 (e8e57c26)
irontile    0.2.5 (b1b3cc7a)
```

Two lines because they are two binaries. Installing a package replaces both on
disk and neither in memory, so a session keeps running whatever it started as
until it is restarted, and those numbers can disagree for days. The second line
is the one that describes the compositor holding your windows.

## Configuration

TOML at `$XDG_CONFIG_HOME/irontile/irontile.toml`, or wherever `--config`
points. There need not be one. `irontilectl reload` re-reads it, and a file that
fails to load leaves the running configuration in place rather than taking the
session down. `irontile --print-config` writes out the defaults.

```toml
[theme]
border_width = 2
border_focused = { colors = ["#ddbba8ee", "#c67f5fee"], angle = 45 }
border_unfocused = "#695959aa"
background = "#121217"
inner_gap = 4
outer_gap = 4
resize_step = 40

[cursor]
theme = "Adwaita"        # an XCursor theme; defaults to $XCURSOR_THEME
size = 24                # logical pixels; a scaled display gets a larger image

# Input devices. Every setting here is handed to libinput, which applies it to
# the device it names -- so a setting left out is not set to false, it is left
# exactly as libinput chose. Running nested these do nothing: the compositor
# underneath has already applied its own.
[input]
natural_scroll = false   # mice, and anything else with a wheel

# Touchpads separately, because wanting one turned around says nothing about
# the other. A touchpad that pushes the page while the wheel is left alone is
# the usual arrangement.
[input.touchpad]
natural_scroll = true
# How a press on a pad with no separate buttons decides which button it was.
# "button-areas" is libinput's default, and reads the answer from where your
# finger is along the bottom edge -- which is why a press low and central can
# arrive as a middle click, closing the browser tab under the pointer.
# "clickfinger" counts fingers instead: one is left, two right, three middle.
click_method = "clickfinger"
tap_to_click = false
middle_button_emulation = false

# The keymap, which is the seat's rather than any one device's: one is compiled
# and handed to every client. Names are xkb's, and an empty one means xkb's own
# default. A keymap that will not compile falls back to the default rather than
# refusing to start -- a mistyped option should not cost you the session you
# would fix it from.
[input.keyboard]
layout = "us"
options = "caps:escape"   # the one setting a binding cannot stand in for:
                          # a binding maps a key to an action, not to another key

# One per display, matched on the connector name the hardware reports. A "*"
# entry applies to any display without one of its own.
[[output]]
name = "DP-4"
position = [0, 0]        # top-left corner in the global logical space

[[output]]
name = "eDP-1"
mode = "2256x1504@60"    # size must match exactly; refresh is matched loosely
scale = 1.3333           # snapped to the nearest 120th, so this is four thirds
transform = "normal"     # or 90, 180, 270, flipped, flipped-90, ...
position = [1156, 1440]
enabled = true

[layout]
smart_split = true       # split along the longer edge, rather than appending
default_axis = "horizontal"
reap_empty_workspaces = true
focus_follows_move = true

# Written on top of the built-in bindings rather than in place of them, so
# adding one key does not mean restating sixty. `default_binds = false` at the
# top level starts from nothing instead.
[binds]
"Super+h" = "focus left"
"Super+Return" = "spawn kitty"
"Super+f" = false        # take away a built-in binding

# A table says what holding the key down does. Ramps -- volume, brightness, a
# resize -- keep going; everything else fires once however long it is held.
"XF86AudioRaiseVolume" = { action = "spawn wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+", repeat = true }
"XF86AudioMute" = "spawn wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle"

# `locked` keeps a binding working while the screen is locked. Off for
# everything by default: the keyboard at a lock screen belongs to the lock
# screen, and a binding that fires anyway can spawn a program behind it.
# Volume and brightness are what this is for.
"XF86AudioLowerVolume" = { action = "spawn wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-", repeat = true, locked = true }
```

**No binding fires while the session is locked** unless it says `locked = true`,
with two exceptions decided in code rather than configuration. Switching virtual
terminal always works: the terminal switched to asks for a login of its own, and
it is the last way out of a session whose lock screen has stopped answering.
Quitting the compositor never works, whatever the file says -- a session started
from a virtual terminal leaves an authenticated shell behind it, so quitting at
the lock screen would hand the machine to whoever pressed the key.

A `spawn` honours quotes, so `spawn sh -c "grim -g \"$(slurp)\""` reaches the
shell as one argument rather than five words. Splitting on spaces alone hands
`sh -c` the first word and throws the rest away, which fails silently: something
runs, just not what was asked for.

Key names are xkb keysyms, so anything `xkbcli` prints works, including the
`XF86` media keys a laptop sends. A binding needs no modifier, which is what
makes those expressible at all. Unknown settings and unparseable bindings are
errors naming the line, not silent no-ops.

Key repeat for a binding is the compositor's own job: an intercepted key never
reaches the client that would normally do the repeating, and the input backend
reports a press and a release with nothing in between. A held binding fires on
a timer at the same delay and rate clients are told to use for typing, so a held
binding and a held letter feel the same. `resize` repeats by default because it
is a ramp; a `spawn` is opaque -- `wpctl set-volume 5%+` is a ramp and `firefox`
is emphatically not -- so it repeats only when the binding says to.

### Bindings

Everything built in is behind Super. Directions are `h`/`j`/`k`/`l` or the
arrow keys.

| Binding | Action |
| --- | --- |
| `Super` + direction | Move focus, crossing to the next display at the edge |
| `Super` `Shift` + direction | Move the window, crossing displays at the edge |
| `Super` `Ctrl` + direction | Resize |
| `Super` `Alt` + direction | Move focus to another display |
| `Super` `Shift` `Alt` + direction | Send this desktop to another display |
| `Super` + `1`–`9`, `0` | Show desktop 1–10, creating it on first use |
| `Super` `Shift` + `1`–`9`, `0` | Send the window to that desktop |
| `Super` `Shift` + `C` | Reload the configuration |
| `Super` + `Q` | Close the window |
| `Super` + `F` | Toggle fullscreen |
| `Super` `Shift` + `Space` | Toggle floating |
| `Super` + `V` / `B` | Split the container vertically / horizontally |
| `Super` + `T` | Flip the container's axis |
| `Super` + `O` | Equalize the container |
| `Super` `Shift` + `E` | Quit |
| three-finger swipe | The desktop left or right of this one. Content follows the fingers |
| `Super` + left-drag | Move a floating window. A tiled one stays where the layout put it |
| `Super` + right-drag | Resize; the edges nearest where the drag started follow the pointer |
| drag a window's edge | Resize; along an edge moves one axis, a corner moves both. The pointer shows the arrow for the edge it is on |

A window escapes tiling with `Super` `Shift` + `Space`, and floats until it is
told otherwise.

**There is no binding for a terminal, deliberately.** The compositor has no
opinion about which one you use, and guessing at a list of names it happens to
know only works until you use one it does not. Bind it like anything else:

```toml
[binds]
"Super+Return" = "spawn kitty"
```

`irontilectl warp <x> <y>` puts the pointer somewhere and `irontilectl click
[1|2|3]` presses a button where it is; `press` and `release` are the same thing
held open, which is the only way to express a drag. The compositor draws the pointer, so it
is the only thing that can move it -- which also means nothing else could drive
one in a test. Everything a pointer reaches is otherwise reachable only by hand,
which is how a bar that received no pointer events at all went unnoticed.

## Control socket

Every binding is also a command:

```
irontilectl focus left
irontilectl workspace 3
irontilectl send-to-output right

irontilectl frame        # where every window is
irontilectl outputs      # displays and their arrangement
irontilectl workspaces   # desktops, and what is on them
irontilectl layers       # panels and overlays, and what they reserve
irontilectl layout       # the whole engine state, as JSON
irontilectl watch        # stream events
```

`IRONTILE_SOCKET` targets a specific instance; otherwise the socket belonging to
`WAYLAND_DISPLAY` is used. Processes irontile spawns inherit both.

## Bar

`irontile-bar` is an ordinary layer-shell client. It opens one bar per display,
reserves an exclusive zone so windows tile below it, and reads the compositor
over the control socket -- so it is not privileged, and killing it leaves the
session running. Start it from `[startup]`.

It is drawn in software, with tiny-skia for the geometry and cosmic-text for
the text. A bar redraws a few hundred pixels a second at most; a GPU context
per display to do that would cost more than it saved.

Configuration is TOML at `$XDG_CONFIG_HOME/irontile/bar.toml`, shaped to follow
waybar because that is what people are porting from: three regions naming
modules, and a table per module carrying its format string, icons and
thresholds. `--dump PATH` renders one bar to a PNG without a compositor, which
is how a configuration is checked without starting a session.

```toml
left = ["window"]
center = ["workspaces"]
right = ["cpu", "battery", "clock"]

[modules.cpu]
type = "command"
format = "{}% {icon}"
icons = ["computer-symbolic"]
color = "#d1c6b4"
warning = 80
critical = 95
interval = 2
command = "..."        # anything that prints a number
```

Module types are `workspaces`, `window`, `clock`, `battery`, `volume`,
`network`, `backlight`, `tray` and `command`. Defining any module of your own replaces
the built-in set, so a region naming something with no table is an error at load
time rather than a silently missing part of the bar.

**Everything a bar draws is in buffer pixels; the configuration is in logical
ones.** Height, padding, icons, the accent line and the text size are each the
configured number times the display's scale. Text is the one rasterized
elsewhere, so it is the one that can be left behind -- and text alone staying
the same number of pixels in a larger buffer reads as the font having shrunk.

**A bar draws at the display's real scale.** It binds `fractional-scale` to
learn the exact number and `viewporter` to say how large the result is meant to
look, then renders a buffer of `logical x scale` pixels and sets the viewport
destination to the logical size. On a 1.3333 panel that is a buffer exactly as
wide as the panel is -- nothing resampled, which is what text needs. Where the
compositor offers no viewporter there is no way to express a fraction, so the
whole-number `preferred_buffer_scale` is used with `set_buffer_scale` instead.

**A panel is told which display it is on, and not told it has left.** A layer
surface belongs to a display outright rather than through a placement, so it was
the one kind of surface that was never told anything: it asked what scale to
draw at, got the focused display's answer, and on a second monitor that is the
wrong one.

**A copy of the screen is rendered rather than read back.** A screenshot is
taken by drawing the display again into an offscreen buffer, from the same
element list that is about to be shown, rather than reading whatever the
hardware last scanned out. That costs a frame's work for something that happens
when somebody presses a key, and in exchange it is one code path on every
backend and asks nothing of the display controller. The copy is made with the
backend's own transform, or the picture comes out upside down -- which is how
this was found.

**A locked session shows the lock screen and nothing else.** Not the windows,
not the panels, not even a bar that reserved space. What is being kept is a
guarantee rather than an appearance: the keyboard goes to the lock screen before
it has drawn anything, because a keystroke reaching a window behind a half-drawn
lock screen is exactly what must not happen, and the pointer can reach nothing
else whether or not anything is visible. A compositor that draws a lock screen
over a live desktop is only pretending.

The session is called locked only once every display is covered by a surface
that has actually drawn -- confirming on the configure instead would say
"locked" while a second monitor still showed what was on it. And the lock
outlives the client: if the locker dies without unlocking, its surfaces go with
it and every display is painted blank, but the session stays locked. A lock
screen that unlocks by crashing is worth less than no lock at all.

**A session says where it is.** A program the compositor starts inherits its
environment and finds the right display. A program started by something else
does not: D-Bus activates a service with the environment the *bus* was given,
and a desktop entry launched through the systemd user manager gets that
*manager's*. Both were set by whichever session started first, and neither hears
about a new one unless told. Untold, a launcher appears on this display and
opens windows on another, which looks like the launcher misbehaving and is
nothing of the sort.

Only the session backend says it -- the compositor that *is* the session is the
one entitled to say where the session is; nested and headless share somebody
else's bus and keep quiet. `XDG_CURRENT_DESKTOP` is deliberately not announced:
it selects a desktop portal backend, and naming one with no backend installed
takes away the file picker rather than improving anything.

```toml
[session]
announce = true              # tell the session bus
announce_to_systemd = false  # and the user manager
```

The second is off by default because that manager is shared by every session
this user has open: setting it while another session is running points that
session's launches here too, and from the outside the two are
indistinguishable. Turn it on when irontile is the only session, which is when
it is the true thing to say.

**A panel gets its frame callbacks back.** A toolkit asks for one and waits for
it before drawing its next frame, so a panel that never receives one draws
exactly once and then stops -- which reads as a client that renders badly rather
than a compositor that never answered. irontile's own bar draws on a schedule of
its own and never asks, so it is the one panel that would never have shown this.

Saying so is only half of it. A panel between sizes has no rectangle for a
moment, and reporting that as "you have left the display" is a thing clients
believe: a toolkit that sizes itself as a share of its monitor then has no
monitor to take a share of. `wofi`, whose default size is half the screen by two
fifths of it, came up a hundred and thirty pixels wide. Going quiet for a frame
says nothing untrue; saying the wrong thing does.

**A bar reuses two buffers and never makes a third.** A buffer handed to the
compositor belongs to the compositor until it says otherwise, so it can neither
be overwritten nor thrown away; making a new one per frame means the compositor
holds every frame the bar has ever drawn. That is invisible while a bar redraws
once a second and fatal when something makes it redraw three hundred times a
second, which is how it was found.

**A panel that hides itself must be told it may come back.** Layer-shell says an
unmapped surface returns to its initial state and may not attach another buffer
until it is configured afresh, so a commit with no buffer -- a surface's first,
or one that has just hidden itself -- is answered with a configure whatever else
is true. Leaving that to the "only when something changed" rule below sends
nothing, because nothing about the configuration did change, and a notification
centre that has been closed waits for ever to be allowed back: it opens once,
closes, and never opens again.

**Nothing tells a client that something changed unless it did.** The compositor
republishes the display arrangement on anything that might have moved a work
area, and a bar's every commit is one of those -- so `reconfigure_outputs`
announces a change only when the arrangement is actually different, and a layer
surface is reconfigured only when its configuration is. Either one on its own is
a loop: the bar redraws because it just drew, as fast as the machine allows.

**A subscriber that falls behind is waited for, not cut off.** Events are
written into a per-peer outbox and pushed out as the socket takes them. Writing
to a full non-blocking socket puts half a frame into the stream and
desynchronizes it permanently; dropping the peer instead means a bar disappears
for being one repaint behind. A peer that stops reading entirely is eventually
let go, because the compositor is the wrong place to store an unbounded amount
of anything on a client's behalf.

**The readings come from the machine, not from other programs.** A battery and
a backlight are files in sysfs; a network link is `/sys/class/net` plus
`/proc/net/wireless` for signal, reported as a share of the seventy the wireless
extensions define. Volume is the one with no file behind it, so it holds a
PulseAudio connection -- which is what `pipewire-pulse` answers -- on a thread
of its own, and writes to a pipe the bar polls alongside the Wayland and control
sockets. That is what makes pressing a volume key show up at once rather than
whenever the next tick comes round. A machine with no sound server draws no
volume module rather than failing.

States that are not levels get their own format: `format_muted` for an output
that is muted, and `format_wifi` / `format_ethernet` / `format_disconnected` for
a link, because the three have nothing to say in common -- a wired link has no
signal and one that is down has no interface worth naming.

**Icons are named, not encoded.** They come from the icon theme by their
freedesktop names -- `battery-good-symbolic`, `network-wired-symbolic` -- and
are rasterized from SVG at the display's scale. An icon font addresses glyphs
by private-use codepoint, which the font is free to renumber in its next
release, and then every icon on the bar is something else. A name does not move.
`icon_path` points at a directory of `<name>.svg` files searched before the
theme, for icons of your own.

`icons` is an array chosen by where a value falls between 0 and 100, so five
icons cover a battery in fifths. Where the icon depends on something that is
not a percentage -- muted, or which kind of link is up -- a command can print
`{icon:name}` itself and the bar resolves it like any other.

A module may set its own `color` and `background`, the way a per-widget rule in
a stylesheet does, and again per state with `color_warning`,
`background_critical`, `color_charging` and so on. Only the pair naming the
state the module is in applies, so a plain `color` chosen for looks still gives
way to the bar's `warning` when a threshold is crossed -- that colour is the
message. Naming `color_warning` is what makes overriding it deliberate.

```toml
[modules.battery]
type = "battery"
color = "#d1c6b4"
color_charging = "#7ab972"       # on the charger, whatever the level
color_critical = "#d1c6b4"       # the field says it, so leave the text alone
background_critical = "#c65f5f"
warning = 30
critical = 15
```

Each bar picks out the desktop on **its own** display rather than the one
holding the keyboard, since with two monitors only one desktop is focused
globally and every other bar would have nothing marked at all.

**A module can say a second thing.** `format_alt` is swapped in when the module
is clicked and back on the next click -- an address rather than an icon, a date
rather than a time. While it is showing, the formats for particular states give
way to it, because asking a network module for the address means the address,
connected by wire or not. A module that names an `on_click` keeps that instead:
a button can only do one thing, and the one written down wins over the one
implied.

**Panels are under the pointer too.** What is under a point is looked for in
the order things are drawn: overlay and top panels, then windows, then the
panels below them. Leaving the panels out means a bar receives no pointer events
at all -- not a click on a desktop button, not the pointer resting on a module,
not even an enter -- and nothing says so.

**A tooltip is a surface of its own.** It has to hang below the bar into the
desktop, and a bar tall enough to contain one would either swallow clicks meant
for the window underneath or need a hole cut in it. It takes no input at all,
because a tooltip that took the pointer would take it off the module it belongs
to -- which would hide the tooltip, which would give the pointer back.

```toml
[tooltip]
delay_ms = 400           # how long the pointer must rest
background = "#1b1918"
foreground = "#d1c6b4"
border = "#413c3a"
border_width = 1
padding = 8
gap = 2                  # between the bar and the tooltip
# font_size = 13         # the bar's, unless you say otherwise

[modules.network]
type = "network"
format_wifi = "{icon}"
format_alt = "{ifname} {signal}%"
tooltip = "{ifname}\nsignal {signal}%"
on_click_right = "nm-connection-editor"
```

A tooltip takes the same placeholders as the module's own format and may run to
several lines; the clock's are strftime, like its format. `--dump PATH --tooltip
TEXT` renders one without a compositor, the same way `--dump` alone renders a
bar.

**The tray is StatusNotifierItem over D-Bus.** Three parties: an application
publishes an item on its own connection, a *watcher* keeps the list, and a
*host* shows them. The watcher is a well-known name, so whoever claims it first
owns the list -- the bar claims it when nothing else has, which is what makes a
tray work on a desktop with no panel from a full environment, and reads
somebody else's list when they got there first.

Items are polled rather than driven by their change signals. A tray holds a
handful of items whose properties are a few hundred bytes, and those signals are
the part of this protocol applications get wrong most often: an item that
forgets to emit one is far more common than a slow tray.

An item may name a themed icon, which costs nothing and follows the icon theme,
or hand over raw ARGB32 pixels. Both work, because the Chromium-based ones --
1Password, Slack -- only do the second. Pixels are scaled once per size and
cached by where they live rather than by what they are called, and they are not
recoloured: that is an application's own artwork, not a symbolic glyph. Left
click activates an item, middle is its secondary action, right asks it for its
menu.

**Right click draws the item's own menu.** An item publishes one over
`com.canonical.dbusmenu` for the host to draw, and the whole tree is fetched at
once: asking level by level is what the protocol expects, but a tray menu is a
handful of rows and a round trip per level costs more than the rows do. A
submenu is descended into rather than opened beside, so there is one surface
however deep it goes.

Two surfaces, not one. The menu is the box; under it sits a transparent sheet
over the whole display, which exists to be clicked on rather than looked at --
a client hears about the pointer only over its own surfaces, so without it there
is no way to know that a click meant "not that, then". The sheet is a single
transparent pixel stretched by a viewport, because a buffer the size of the
screen would cost megabytes to say nothing, and it holds the keyboard so that
Escape closes the menu.

```toml
[menu]
background = "#1b1918"
foreground = "#d1c6b4"
disabled = "#695959"
highlight = "#413c3a"
highlight_foreground = "#ffffff"
border = "#413c3a"
border_width = 1
padding = 8
min_width = 180
```

## Lock screen

`irontile-lock` is an ordinary `ext-session-lock` client with no privileges of
its own: it covers every display, asks PAM about a password, and unlocks. Bind
it, or hand it to an idle daemon.

Three pieces that do not know about each other. The drawing knows nothing about
Wayland, so `--dump PATH --size WxH --scale N` renders every state of the screen
to a PNG -- resting, typing, checking, refused, accepted -- which is the only
way to work on a lock screen without locking the machine and hoping. The
authentication knows nothing about drawing, and `--verify` runs it alone from a
terminal. The session half holds the displays and drives the other two.

**PAM runs on its own thread.** It takes a couple of seconds to refuse, which is
deliberate and is what makes guessing expensive. Asking it on the loop that
draws would stop the clock and the keyboard for that whole time -- a lock screen
that appears to have crashed at exactly the moment somebody is anxious about it.
The answer comes back over a channel, and a pipe wakes the loop that is asleep
on file descriptors.

**`--daemonize` forks once the screens are actually covered**, and not a
moment before. An idle daemon told to lock before suspending waits for its lock
command to finish -- that wait is the whole guarantee -- so a locker that stays
in the foreground until somebody unlocks it would hold it open forever and the
machine would sit awake on a lock screen with the lid shut. The two halves share
nothing but a pipe, made before anything is connected: the half that carries on
writes one byte to it when the compositor reports the session locked, and the
half that waits exits on that byte, or non-zero if the pipe closes without one.
Forking a live Wayland connection instead would leave two processes holding one
socket and one sequence of object ids.

**The battery sits under the hostname, in the corner that says what machine
this is.** It is the one thing on the lock screen that cannot be found out any
other way: the bar that would otherwise show it is behind the very screen you
are looking at, and a laptop locked by an idle timer spends its time suspended
with the lid shut. So the corner answers the question you actually walk up with
-- is it charging, and did it survive. The glyph is drawn from rectangles
rather than set from a font, because a battery is exactly the character a bare
machine turns out not to have, and a lock screen is the last place to find that
out. On the charger the case fills and takes a bolt; under fifteen percent
unplugged, the case and the number go red; on a machine with no battery at all
nothing is drawn, which is not the same picture as nought percent.

**The field shows a fixed number of pips, and never grows.** A track that grew
with the entry would shift as it was typed and would tell anyone watching how
long the password is. One pip brighter than the rest, walking round as keys
land, says a key registered without saying how many have.

**It draws when the compositor asks for a frame**, so a locked laptop with its
displays asleep does no work at all: the clock stops because nobody can see it.
A backstop redraws anyway if no frame is asked for within a couple of seconds,
because a compositor going quiet without turning the displays off would
otherwise stop the clock in front of somebody.

## Protocols

| Protocol | Notes |
| --- | --- |
| `wl_compositor`, `wl_shm`, `wl_seat`, `wl_output`, `xdg_output` | |
| `xdg_shell` | Toplevels and popups, with grabs, so menus dismiss |
| `xdg-decoration` | Every request is answered server-side; clients never draw their own titlebars |
| `wlr-layer-shell` | Bars and panels; exclusive zones shrink the work area windows tile into |
| `wl_data_device`, `primary-selection` | Clipboard and middle-click paste. Both hand the selection to whichever client holds keyboard focus, and to no other |
| `wlr-data-control`, `ext-data-control` | The clipboard without focus, for the tools that have no window to focus: a clipboard manager watching for changes, or a paste helper asked for the contents. Given the choice they use this; without it they map a one-pixel window to take the focus instead, which a tiling compositor dutifully tiles. Any client may use it. The filter could gate on a client's pid, but that is a thin fence: pids are reused, a script's is its interpreter's, and anything the user can run could be named. Telling a sandboxed application from the user's own clipboard manager properly wants `security-context`, which is not implemented here |
| `cursor-shape` | Clients name a cursor and the compositor supplies the image, from an XCursor theme |
| `linux-dmabuf` | Clients hand over GPU buffers instead of rendering into shared memory. Advertised only when there is a renderer, so never headless. On a session it carries feedback naming the render node, without which clients cannot pick a GPU and fall back to the CPU |
| `wlr-screencopy` | Copies a display to a client. Screenshots today, screen sharing once a portal sits on top. Any client may ask, so the compositor draws an orange dot in the corner of every display while copies are being served: it cannot stop something that reaches the socket from taking a picture -- anything that can is already running as you -- but it can refuse to let it happen quietly. The mark is drawn above everything, including a fullscreen window and the lock screen, so the client being marked cannot cover it, and it is in the list screencopy renders from, so a recording carries it. It is not themeable: a warning light nobody can turn off is the only kind worth having |
| `ext-session-lock` | Locks the session: one surface per display, and nothing behind them reachable |
| `ext-idle-notify` | How long the seat has been idle, which is what an idle daemon waits on to dim, lock or suspend |
| `idle-inhibit` | A window asking the session to stay awake, honoured only while it is on screen |
| `fractional-scale`, `viewporter` | A client is told the exact scale of the display it is on, so it can render at 1.5x rather than at 2x and be resampled down. The two go together: without viewporter there is no way to say how large a 1.5x buffer should appear. Layer surfaces are told too, which is what keeps a bar's text sharp |

The pointer is drawn by the compositor, because on real hardware nothing else
will. A client that supplies its own cursor surface gets that; one that names a
shape gets it from an XCursor theme; and a built-in arrow covers the case where
no theme is installed, which is what a fresh machine looks like.

Not yet implemented: XWayland, `pointer-constraints` and `relative-pointer`.

## Backends

| Backend | What it is |
| --- | --- |
| nested | A window inside another compositor. The development loop. |
| headless | No renderer, displays described on the command line. What the integration tests drive over the control socket. |
| session | Real hardware: libseat for the seat, udev for GPUs, one `DrmCompositor` per connected connector, libinput for input. |

## Logging

`RUST_LOG=irontile=debug` logs what is worth knowing when there is no other way
to see anything: `display lit`, an `alive` heartbeat every five seconds, each
reflow's placements, and keypresses aimed at the compositor with the keysym and
whether a binding matched.

Only presses holding Super, Control or Alt are logged, plus any that fired a
binding. Plain typing is never recorded at any level — turning on debug logging
must not turn the compositor into a keylogger. Window titles, app ids, spawn
arguments and clipboard contents are not logged either.

`RUST_LOG=irontile=trace` adds the page-flip cycle, `queued a frame` and
`vblank` in pairs. That is two lines per frame per display, so it is for
diagnosing a stalled display rather than for leaving on.

A session also writes `~/.local/state/irontile/irontile.log`, keeping the run
before it as `irontile.log.1`. A compositor started from a virtual terminal
writes to that terminal, and nobody reads a virtual terminal afterwards: the
session worth reading about is usually the one whose output went with the
reboot. Nested and headless runs do not write it -- they are started from a
terminal somebody is already watching, and writing the same path would rotate
away the log of the session they are nested inside.

## Building from source

Everything above is about running irontile. This is what it takes to work on
it.

### Tasks

```
cargo xtask run      # build and launch the compositor
cargo xtask test     # rustfmt check, clippy with warnings denied, full test suite
```

The Rust version is pinned exactly in `rust-toolchain.toml`, so rustup fetches
the same compiler CI uses and a new Rust release cannot break the build without
a commit that says so. `rust-version` in `Cargo.toml` is a separate promise: the
oldest compiler the code still builds on, which the `msrv` job checks.

### Running it

irontile nests if `WAYLAND_DISPLAY` or `DISPLAY` is set and takes the session
otherwise, which is what each of those situations means. `--nested`,
`--session` and `--headless` override the choice.

Nested, irontile opens as a window inside your current compositor and prints
the Wayland socket it bound. Clients pointed at that
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

### On real hardware

The session backend runs: it modesets, tiles, takes input and exits cleanly.
Still test it from a **second VT** rather than by quitting the session you have,
so that one stays there to switch back to.

```
# Ctrl+Alt+F2, log in, then:
cd path/to/irontile
./try-session.sh
```

That wrapper runs irontile under a timeout, so the machine comes back on its own
whatever happens, and logs to `~/irontile-session.log` where it is readable from
your other session afterwards.

It also gives the session a D-Bus session bus of its own. A second graphical
session for the same user otherwise shares the first one's, and a notification
daemon can only own its name once: started without a bus of its own, swaync
finds the name taken and exits, and `swaync-client` then reaches the instance
belonging to the *other* session -- which draws its panel on the other VT, while
the binding that asked for it appears to do nothing. Everything on the session
bus behaves that way, media players found by playerctl included. A bus per
session is what makes it a session rather than a program sharing somebody
else's. `IRONTILE_SHARE_BUS=1` uses the outer one instead.

The display goes into that environment before the bus starts, not just into the
compositor's. A program started by D-Bus activation rather than by the
compositor inherits the *bus daemon's* environment, and on a bare VT that has no
`WAYLAND_DISPLAY` at all -- so a notification daemon that dies and is activated
again has no compositor to draw on, and never appears anywhere. It works once,
and then never again, which reads as a compositor bug and is not one. `Ctrl+Alt+F<n>` switches away at any point, and
`pkill -x irontile` from there stops it.

A compositor holding the VT in graphics mode is the only thing that can perform
a VT switch — the kernel stops handling it — so `Ctrl+Alt+F1` through `F12` are
bound for exactly that, outside the Super prefix everything else uses.

Put something in `[startup]` before the first run. With no windows, a working
compositor and a broken one both show a background colour:

```toml
[startup]
exec = ["alacritty"]
```

### The crates

| Crate | What it is |
| --- | --- |
| `irontile-layout` | Tiling tree, desktops, and display arrangement. Pure integer geometry, no Wayland. |
| `irontile-comp` | The compositor. Owns every protocol and rendering concern. |
| `irontile-ipc` | Control-socket protocol, the shared action vocabulary, and `irontilectl`. |
| `irontile-ui` | Bar and launcher, as ordinary layer-shell clients. |
| `irontile-lock` | `irontile-lock`, the lock screen. Draws, and asks PAM. |
| `irontile-session` | `start-irontile`, the supervisor a login manager starts. |
| `xtask` | Task runner. |

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

**A border may be a gradient, and it is measured across the window.** A colour
is either one `"#rrggbbaa"` or a table of `colors` and an `angle`, which is the
shape the thing being replaced already used. The angle is CSS's -- zero points
up, and it turns clockwise -- so 45 runs from the bottom-left corner to the
top-right, and the four sides meet at the corners rather than each running
through the colours on its own. It is drawn as a run of solid quads, each
filled with the gradient at its own centre, because a per-pixel gradient would
want a shader of its own and on a strip two pixels thick the difference does
not survive being looked at.

**Colours are held straight and premultiplied on the way out.** Interpolating
two half-transparent stops has to happen before the multiply or the result is
pulled toward whichever end is more opaque; the renderer blends assuming
premultiplied, so handing it a straight colour paints a see-through border far
too bright. At full alpha the two are identical, which is why this is the kind
of mistake that only appears the first time someone writes one.

**A border is a border, not a backdrop.** It is drawn as four strips around a
window rather than a filled quad behind one. The difference shows only while a
client's buffer is smaller than its cell -- during a resize, or in a new
window's first frames -- and then a backdrop paints that gap border-coloured,
which reads as a flash.

**A window is told its cell before it paints.** A toplevel joins the tree as
soon as it appears, so its first configure carries the size it will actually
occupy, but it is held out of the frame until it has committed a buffer. Either
half alone is visible: configured late, it paints at the wrong size and snaps;
rendered early, an empty bordered rectangle sits on screen until it draws.

**Decoration is not the client's decision.** The compositor draws a border
rectangle and nothing else, so every `xdg-decoration` request is answered
server-side whatever it asked for. Without the protocol advertised at all,
toolkits fall back to drawing their own titlebars and shadows, which is worse
than either choice made deliberately.

**A panel can be asked about.** A layer surface is neither a window nor a
display, so nothing else the socket reports describes one -- which leaves a bar,
a notification or a lock screen that fails to appear with nothing to look at but
the screen it is not on. `irontilectl layers` gives each one's namespace, its
stratum, where it sits, what it reserves and whether it holds the keyboard.

**The work area is what layer-shell leaves behind.** A bar reserving thirty
pixels at the top of a display shrinks that display's work area, and the layout
engine tiles into what is left. The engine never learns that layer-shell exists;
it is handed a rectangle.

**Scales are snapped to 120ths.** That is the granularity the fractional-scale
protocol can express, so rounding to it means the compositor lays windows out at
exactly the number clients were told, and it turns the approximations people
write into the values they meant: `1.3333` is four thirds.

**Physical pixels and logical pixels are kept apart.** A display scans out a
mode; windows are laid out in that divided by its scale. Conflating the two is
what makes a scaled display either tiny or blurry, and the failure is invisible
at scale one, so it only shows up on a second monitor.

**A display keeps its identity across unplugging.** Connector names map to
output ids for the life of the session, so a monitor that comes back is the same
display as far as the layout engine is concerned — which is what makes the
desktop that preferred it return to it.


**The renderer lives in the compositor state**, not in a backend's event loop,
which is what lets a client's dmabuf be imported at the moment it is submitted:
the import needs the renderer, and the protocol handler only has the
compositor.

**The session backend is paced by the display**, not by a timer. A frame is
queued, a page flip completes, and that vblank asks for the next one; a display
with nothing to draw goes quiet.

## Releases

Versions are the workspace `version` in `Cargo.toml`, and a release is that
version tagged `v0.1.0`. The tag is the thing packaging builds from, so it moves
only forward and never moves once pushed.

## License

MIT. See [LICENSE](LICENSE).
