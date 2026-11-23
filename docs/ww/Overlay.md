# The overlay layer (control API `overlay`)

ww_niri draws one overlay layer above the windows for its control API clients. Each client puts up items there: a blurred cover over a window, a line of text, a clock, a notice, a meter. ww_niri puts up one item of its own: the volume indicator. Items of different clients never replace each other: each belongs to the connection that put it up, and they stack by priority. pult_adwatch's ad cover and pult_hub's exit-switch text are items; so can be a clock in a corner or a notice from another service.

## Ownership

- An item belongs to the connection that put it up: its owner. The owner names it with a key of its own (`"ad-cover"`, `"exit-switch"`), at most 64 bytes.
- `overlay.put(key, item)` puts the item up, or changes the owner's item of that key: its content, place, priority and end. The same key of another owner is another item.
- `put` answers the item's `ItemId`, server-wide and never reused while the compositor runs. A change keeps the id.
- `overlay.lift(key)` lifts the owner's own item. No call lifts another owner's item, except `dismiss`.
- `overlay.dismiss(id)` lifts any item put up as `dismissible`. It is a person's "show it", from any client. The owner hears `Dismissed` and decides what follows (pult_adwatch shows the break to its end).
- When the owner's link ends, its items lift (`Backend::end` of the connection). A reconnect is a new owner: the client puts its items up again.
- Limits: 16 items per owner, 128 in all (`TooMany`). Texts at most 500 bytes.

## Lifetime

- `lasts` (1 ms to 15 min): the item lifts by itself at the end; a `put` of the same key moves the end. A hung client that keeps its link still loses its items.
- `lasts: None`: the item stays until it is lifted, dismissed or its owner's link ends. For a clock or a status line.
- An item on a window lifts when the window closes.
- Every item that lifts is announced to every client: `Event::OverlayEnded { id, why }`, `why` one of `Deadline`, `Lifted`, `Dismissed`, `Disconnected`, `TargetGone`. A change by `put` is not an end.

## Places

A place is a surface and where on it:

| `on` | Surface |
|---|---|
| `Window(id)` | The window: the item moves, scales and hides with it (other workspace, `windows.hide`, the overview zooms it). |
| `Slot(slot)` | The window of a browser slot, found at each `put`; `NoSuchWindow` while it has none. |
| `Output(name)` | An output by connector name; `None` is the focused output. Drawn only while that output exists. |

| `at` | Where |
|---|---|
| `Fill` | The whole surface. The content is centred on it. |
| `Anchor { anchor, dx, dy }` | A corner, an edge's middle or the centre (`TopLeft` .. `BottomRight`). The item's own point of the same name sits there, moved by `dx`, `dy` logical pixels: `BottomRight` with `dx: -40, dy: -40` is 40 px in from the bottom right corner. The item is as large as its content. |

Sizes of text are relative: they are laid out for a surface 1080 px high and scaled to the real one, so a card looks the same on a 1080p and a 4K TV and on a window of any size.

## Kinds

| `content` | Draws |
|---|---|
| `Card { title, countdown, clock, footer }` | The cover's card: a large title, a countdown to the item's end (`m:ss`), a large clock, a small footer. Only with `Fill`. |
| `Text { text, size }` | Lines of text, `Small`, `Medium` or `Large`. |
| `Clock { size }` | `HH:MM` in the compositor's local time. |
| `Notice { title, body }` | A bold title and a smaller body, in a box. |
| `Progress { title, detail }` | A bold title, a smaller detail and the time since the item's first put (`m:ss`), counting up once a second. A `put` of the same key keeps the start: a mark that turns into a progress counts from the mark. |
| `Meter { label, percent }` | A bold label, then a bar of 10 blocks filled to the nearest tenth and the percent (0 to 999); past 100 % the bar stays full. |

A progress counts on the compositor's side: a client that hangs and stops renewing its item cannot leave a "loading" that looks alive. Give it a `lasts` and renew it while the work runs; it lifts by itself when the renewals stop.

Every item has a backdrop under its content: `Backdrop { blur, dim, fill }`, from the bottom the fill, the blur, the dim.

- `fill`: what the box is filled with. `None`: nothing, what is under the box shows. `Color(Rgba)`: a colour, its alpha 0 (clear) to 255 (opaque). `Image(name)`: a PNG image of ww_niri's backgrounds folder, scaled to cover the box and cut to it, its middle in the box's middle.
- `blur`: 0 (none) to 4 passes of niri's dual-kawase blur, over the fill.
- `dim`: 0 to 100 % of a warm dark over the blur.
- An anchored item's backdrop is a box around its content; a `Fill` item's is the whole surface.
- No backdrop, the picture as it is: `blur: 0, dim: 0, fill: None`.

Images are never sent over the API: a `put` names a file of the backgrounds folder, `backgrounds` in `ww.kdl` (`backgrounds` next to `ww.kdl` when left out). A name is 1 to 64 bytes of ASCII letters, digits, `-`, `_` and `.`, not starting with `.`, so never a path (`BadArgument`). A missing file, one past 32 MiB or 16.6 million pixels, or one that is not a PNG image is `NoSuchImage`. The file is read at the `put` and kept while an item uses it; a `put` after the file changed reads it again.

`Fill` on a window with `blur` above 0 is a cover: the window itself is not drawn, only its blurred copy under the fill, and its popups are hidden. The copy is the window rendered at a quarter of the output scale and blurred, reblurred only when the window's content changes. When the blur fails, the backdrop is fully opaque: nothing of a covered window shows. Any other item is drawn over what is below it; its blur blurs what is under its box on the screen (niri's framebuffer effect, as for layer surfaces).

`mute: true` mutes the default sink while the item is up. When the last muting item lifts, the sink gets back the mute it had before the first. Without an audio server the item still shows.

## Shown

`report_shown: true` on a `put` asks when the item reaches the screen. The owner alone gets `Event::OverlayShown { id, at }` once, for the first frame that drew that put: `at` is the frame's presentation time from niri's presentation feedback (the DRM vblank), in microseconds on the compositor's `CLOCK_MONOTONIC`. A client on the same machine compares it with its own monotonic clock to measure request to screen. Each `put` that asks is reported again; a `put` that does not ask costs nothing.

- Only frames for the screen count: a screencast or screenshot render does not.
- An item that is never drawn (its output gone, a window on another workspace, the screen locked) is never reported.
- A `put` queues a redraw of the outputs at once; the next frame draws it, without waiting for the overlay's timer.

## Show only

`overlay.show_only` is one filter for the whole layer, set by any client: `None` draws every item, `Some(ShowOnly { bands, keys, ids })` only the items matching one of its parts.

- `bands`: priorities from `lowest` to `highest`, both included: `{ lowest: 192, highest: 255 }` shows the status of what plays alone.
- `keys`: owners' keys, whoever the owner is: `"status"` matches every owner's `status` item.
- `ids`: items by id, as `list` shows them.
- At most 16 of each, keys as for `put`; a band upside down is `BadArgument`.

Hidden items stay up: they keep their end, their mute, their `OverlayEnded`, and a cover hides its window only while drawn. `list` marks them `hidden`. A hidden item is not drawn, so a `report_shown` put is reported when the filter first lets it show. The filter is an observed property: every client observing it hears each change.

## The volume indicator

ww_niri shows the default sink's volume on every change of its volume or mute, from any client, key or tool (`wpctl`), for 1.5 s after the last change: a `Meter` with `Volume` (or `Muted`) and the percent. It is an item of its own, owned by the compositor, with key `volume` and priority 224: it never takes another owner's place in the stack, and no client can lift it, though `show_only` can hide it. A change of the default sink itself does not show it, nor does the mute of an item with `mute`.

Its place is the `volume-indicator` line of `ww.kdl`, each property optional:

```kdl
volume-indicator anchor="top-right" dx=-40 dy=40 output="HDMI-A-1" label="Громкость" muted-label="Без звука"
```

Without the line it is top right, 40 px in from both edges, on the focused output.

## Stacking

- `priority` 0 to 255; higher is drawn on top. Equal priorities: the later item (higher id) on top. A change by `put` keeps an item's place in the stack, unless it changes the priority.
- Bands, by convention: 0-63 ambient (a clock), 64-127 notices, 128-191 covers, 192-255 status about what plays (an exit switch). A status over a cover is how a person learns why the picture stalls while an ad is covered.
- Items never merge: two covers on one window are two items; the window is drawn once, blurred by the topmost cover's strength, and each item's dim and content stacks.

## In niri's render path

- Window items are drawn by the window's tile, in its render elements: they follow the tile's moves, resizes, opacity and the overview, and leave with it. They are drawn over the window and its popups.
- Output items are drawn in `Niri::render` above the layer shell's overlay layer and the windows, under niri's own UI (the Alt-Tab switcher, the hotkey overlay, the exit dialog, the config error notice) and the pointer. They are not drawn while the screen is locked or the screenshot UI is open.
- Each item's textures are kept between frames and drawn again only when their text or size changes: once a second for a countdown or a progress, once a minute for a clock. One timer wakes the loop at the next end, countdown or progress second, or minute.

## The API

```rust
ww_impl!(overlay: Overlay);

trait Overlay {
    fn put(key: &str, item: Item) -> Result<ItemId, OverlayError>;
    fn lift(key: &str) -> Result<(), OverlayError>;
    fn dismiss(id: ItemId) -> Result<(), OverlayError>;
    /// Every item, `own` set on the caller's, `hidden` on those show_only hides.
    fn list() -> Vec<ItemInfo>;
    property!(rw+observe show_only: Option<ShowOnly>, OverlayError);
}
```

`OverlayError`: `NoSuchWindow`, `NoSuchItem`, `NotDismissible`, `TooMany`, `BadArgument` (a `lasts`, blur, dim, meter percent or text out of range, a `Card` not on `Fill`, a key empty or too long, an image name that could be a path, a `show_only` past its limits), `NoSuchImage`.

`ww_niri_client` puts and lifts through commands of one lane per key (`Command::OverlayPut`, `OverlayLift`, `OverlayDismiss`): a `put` queued behind one of the same key replaces it. `Tv::overlay_ended` tells the ends of every item, with the key of the client's own; the client's own items end as `Disconnected` when its link goes. `Tv::overlay_shown` tells when the client's items asking `report_shown` reached the screen. `tv.overlay().show_only(filter)` sets the filter, latest-wins. `ww_niri_fake` keeps the same items and rules, for the tests of clients; it has no frames and reports a `report_shown` put shown at once, at its clock's time. Its backgrounds folder is the names `FakeTv::add_background` adds, and it puts up the same volume indicator, top right.

## Why this shape

- Per-window covers had one cover per window: the last `cover` replaced the card and took the lease, so pult_hub could not put its exit-switch text on mpv's window without lifting or hijacking pult_adwatch's ad cover. Items of their own owners stack instead.
- Keys chosen by the owner make a renewal idempotent: pult_adwatch puts its cover again every 3 s without keeping ids, and a renewal queued twice is one call.
- Server ids are for the others: a person's dismiss names an item it saw in `list`, not another client's key.
- The owner is the connection, as for held keys and buttons: no client has to clean up after a crash, and no client can lift what another holds by mistake.
- One `show_only` for all: it is what the screen shows, which every client shares; a per-client filter would let two clients disagree about one screen.
- Fill images by name only: no bytes from the network reach an image decoder, and a background shared by many items is read once.
