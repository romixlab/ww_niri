# The overlay layer (control API `overlay`)

ww_niri draws one overlay layer above the windows for its control API clients. Each client puts up items there: a blurred cover over a window, a line of text, a clock, a notice. Items of different clients never replace each other: each belongs to the connection that put it up, and they stack by priority. pult_adwatch's ad cover and pult_hub's exit-switch text are items; so can be a clock in a corner or a notice from another service.

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

Every item has a backdrop under its content: `Backdrop { blur, dim }`.

- `blur`: 0 (none) to 4 passes of niri's dual-kawase blur.
- `dim`: 0 to 100 % of a warm dark over the blur.
- An anchored item's backdrop is a box around its content; a `Fill` item's is the whole surface.

`Fill` on a window with `blur` above 0 is a cover: the window itself is not drawn, only its blurred copy, and its popups are hidden. The copy is the window rendered at a quarter of the output scale and blurred, reblurred only when the window's content changes. When the blur fails, the backdrop is fully opaque: nothing of a covered window shows. Any other item is drawn over what is below it; its blur blurs what is under its box on the screen (niri's framebuffer effect, as for layer surfaces).

`mute: true` mutes the default sink while the item is up. When the last muting item lifts, the sink gets back the mute it had before the first. Without an audio server the item still shows.

## Stacking

- `priority` 0 to 255; higher is drawn on top. Equal priorities: the later item (higher id) on top. A change by `put` keeps an item's place in the stack, unless it changes the priority.
- Bands, by convention: 0-63 ambient (a clock), 64-127 notices, 128-191 covers, 192-255 status about what plays (an exit switch). A status over a cover is how a person learns why the picture stalls while an ad is covered.
- Items never merge: two covers on one window are two items; the window is drawn once, blurred by the topmost cover's strength, and each item's dim and content stacks.

## In niri's render path

- Window items are drawn by the window's tile, in its render elements: they follow the tile's moves, resizes, opacity and the overview, and leave with it. They are drawn over the window and its popups.
- Output items are drawn in `Niri::render` above the layer shell's overlay layer and the windows, under niri's own UI (the Alt-Tab switcher, the hotkey overlay, the exit dialog, the config error notice) and the pointer. They are not drawn while the screen is locked or the screenshot UI is open.
- Each item's textures are kept between frames and drawn again only when their text or size changes: once a second for a countdown, once a minute for a clock. One timer wakes the loop at the next end, countdown second or minute.

## The API

```rust
ww_impl!(overlay: Overlay);

trait Overlay {
    fn put(key: &str, item: Item) -> Result<ItemId, OverlayError>;
    fn lift(key: &str) -> Result<(), OverlayError>;
    fn dismiss(id: ItemId) -> Result<(), OverlayError>;
    /// Every item, `own` set on the caller's.
    fn list() -> Vec<ItemInfo>;
}
```

`OverlayError`: `NoSuchWindow`, `NoSuchItem`, `NotDismissible`, `TooMany`, `BadArgument` (a `lasts`, blur, dim or text out of range, a `Card` not on `Fill`, a key empty or too long).

`ww_niri_client` puts and lifts through commands of one lane per key (`Command::OverlayPut`, `OverlayLift`, `OverlayDismiss`): a `put` queued behind one of the same key replaces it. `Tv::overlay_ended` tells the ends of every item, with the key of the client's own; the client's own items end as `Disconnected` when its link goes. `ww_niri_fake` keeps the same items and rules, for the tests of clients.

## Why this shape

- Per-window covers had one cover per window: the last `cover` replaced the card and took the lease, so pult_hub could not put its exit-switch text on mpv's window without lifting or hijacking pult_adwatch's ad cover. Items of their own owners stack instead.
- Keys chosen by the owner make a renewal idempotent: pult_adwatch puts its cover again every 3 s without keeping ids, and a renewal queued twice is one call.
- Server ids are for the others: a person's dismiss names an item it saw in `list`, not another client's key.
- The owner is the connection, as for held keys and buttons: no client has to clean up after a crash, and no client can lift what another holds by mistake.
