//! The overlay: items of several owners stack, each owner lifts only its own, and an item lifts
//! at its end, on a dismiss, with its window and with its owner's link.

use std::time::Duration;

use exact_units::Fixed;
use wire_weaver_client::CallError;
use ww_niri_api::{
    Anchor, At, Backdrop, BrowserSlot, CardOwned, ContentOwned, EventOwned, ItemEnd, ItemId,
    ItemOwned, OverlayError, PlaceOwned, Px, SurfaceOwned, TextSize, WindowId,
};

use super::{connect, fake, run, soon, NiriClient};

pub fn card() -> CardOwned {
    CardOwned {
        title: "Реклама".to_owned(),
        countdown: Some("Передача вернётся через".to_owned()),
        clock: true,
        footer: Some("Первый канал".to_owned()),
    }
}

/// A dismissible ad cover over `on` for `lasts_ms`.
pub fn cover(on: SurfaceOwned, lasts_ms: u32, mute: bool) -> ItemOwned {
    ItemOwned {
        place: PlaceOwned { on, at: At::Fill },
        content: ContentOwned::Card(card()),
        backdrop: Backdrop { blur: 2, dim: 65 },
        priority: 150,
        lasts: Some(Fixed::new(lasts_ms)),
        mute,
        dismissible: true,
    }
}

/// A status line at the bottom of window `id`, until lifted.
pub fn text(id: u64) -> ItemOwned {
    ItemOwned {
        place: PlaceOwned {
            on: SurfaceOwned::Window(WindowId(id)),
            at: At::Anchor {
                anchor: Anchor::Bottom,
                dx: Px::new(0),
                dy: Px::new(-80),
            },
        },
        content: ContentOwned::Text {
            text: "Переключаю выход".to_owned(),
            size: TextSize::Medium,
        },
        backdrop: Backdrop { blur: 0, dim: 60 },
        priority: 200,
        lasts: None,
        mute: false,
        dismissible: false,
    }
}

fn window(id: u64) -> SurfaceOwned {
    SurfaceOwned::Window(WindowId(id))
}

fn device<T: std::fmt::Debug>(res: Result<T, CallError<OverlayError>>) -> OverlayError {
    match res {
        Err(CallError::Device(err)) => err,
        other => panic!("not a device error: {other:?}"),
    }
}

async fn put(client: &NiriClient, key: &str, item: ItemOwned) -> ItemId {
    (client.overlay().put(key.to_owned(), item).call().await).unwrap()
}

#[test]
fn two_owners_items_stack_and_never_replace_each_other() {
    let tv = fake();
    run(async {
        let a = connect(&tv).await.unwrap();
        let b = connect(&tv).await.unwrap();
        let ad = put(&a, "ad", cover(window(2), 60_000, false)).await;
        // The same key of another owner is another item, on the same window.
        let b_ad = put(&b, "ad", cover(window(2), 60_000, false)).await;
        let exit = put(&b, "exit", text(2)).await;
        assert_ne!(ad, b_ad);
        let stack =
            |tv: &ww_niri_fake::FakeTv| tv.overlay().iter().map(|i| i.id).collect::<Vec<_>>();
        assert_eq!(stack(&tv), [exit, b_ad, ad]);

        // A change keeps the id and the other items.
        assert_eq!(put(&a, "ad", cover(window(2), 30_000, false)).await, ad);
        assert_eq!(stack(&tv), [exit, b_ad, ad]);

        let listed = a.overlay().list().call().await.unwrap();
        let seen: Vec<_> = listed
            .iter()
            .map(|i| (i.id, i.key.as_str(), i.own))
            .collect();
        assert_eq!(
            seen,
            [(exit, "exit", false), (b_ad, "ad", false), (ad, "ad", true)]
        );

        // An owner lifts only its own: the key names its own item.
        assert_eq!(
            device(a.overlay().lift("exit".to_owned()).call().await),
            OverlayError::NoSuchItem
        );
        a.overlay().lift("ad".to_owned()).call().await.unwrap();
        assert_eq!(stack(&tv), [exit, b_ad]);
    });
}

#[test]
fn items_lift_by_end_lift_dismiss_and_window() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        let mut events = client.events().call().await.unwrap();
        let mut next = async || soon("an event", events.recv()).await.unwrap();
        let ended = |id, why| EventOwned::OverlayEnded { id, why };

        // Lifts at its end; a change moves the end.
        let ad = put(&client, "ad", cover(window(2), 10_000, false)).await;
        tv.advance(Duration::from_secs(8));
        put(&client, "ad", cover(window(2), 5_000, false)).await;
        assert_eq!(tv.overlay()[0].until_us, Some(13_000_000));
        tv.advance(Duration::from_secs(5));
        assert_eq!(next().await, ended(ad, ItemEnd::Deadline));
        assert!(tv.overlay().is_empty());

        // Lifted, dismissed, or not dismissible.
        let ad = put(&client, "ad", cover(window(2), 10_000, false)).await;
        client.overlay().lift("ad".to_owned()).call().await.unwrap();
        assert_eq!(next().await, ended(ad, ItemEnd::Lifted));
        let ad = put(&client, "ad", cover(window(2), 10_000, false)).await;
        client.overlay().dismiss(ad).call().await.unwrap();
        assert_eq!(next().await, ended(ad, ItemEnd::Dismissed));
        let exit = put(&client, "exit", text(2)).await;
        assert_eq!(
            device(client.overlay().dismiss(exit).call().await),
            OverlayError::NotDismissible
        );

        // Refused.
        let o = client.overlay();
        let refused = async |key: &str, item| device(o.put(key.to_owned(), item).call().await);
        assert_eq!(
            refused("ad", cover(window(99), 1000, false)).await,
            OverlayError::NoSuchWindow
        );
        assert_eq!(
            refused("ad", cover(window(2), 0, false)).await,
            OverlayError::BadArgument
        );
        assert_eq!(
            refused("", cover(window(2), 1000, false)).await,
            OverlayError::BadArgument
        );
        assert_eq!(
            refused("ad", cover(SurfaceOwned::Slot(BrowserSlot(0)), 1000, false)).await,
            OverlayError::NoSuchWindow
        );

        // The window closes under it.
        tv.close_window(WindowId(2)).unwrap();
        loop {
            match next().await {
                EventOwned::OverlayEnded { id, why } => {
                    assert_eq!((id, why), (exit, ItemEnd::TargetGone));
                    break;
                }
                EventOwned::WindowClosed(_) | EventOwned::WindowFocusChanged(_) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(tv.overlay().is_empty());
    });
}

#[test]
fn a_slot_item_goes_on_the_slots_window() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        client.browsers(0).start(true).call().await.unwrap();
        tv.advance(Duration::from_secs(1));
        let state = client.browsers(0).read_state().read().await.unwrap();
        put(
            &client,
            "ad",
            cover(SurfaceOwned::Slot(BrowserSlot(0)), 1000, false),
        )
        .await;
        assert_eq!(tv.overlay()[0].window, state.window);
    });
}

#[test]
fn muting_items_hold_the_mute() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        let audio = client.audio();
        let muted = async || audio.read_muted().read().await.unwrap();
        let lift = async |key: &str| {
            (client.overlay().lift(key.to_owned()).call().await).unwrap();
        };

        // Muted with the first muting item, back when the last one lifts.
        put(&client, "a", cover(window(1), 10_000, false)).await;
        assert!(!muted().await);
        put(&client, "a", cover(window(1), 10_000, true)).await;
        put(&client, "b", cover(window(2), 20_000, true)).await;
        assert!(muted().await);
        tv.advance(Duration::from_secs(10));
        assert!(muted().await);
        lift("b").await;
        assert!(!muted().await);

        // A user's unmute holds through changes.
        put(&client, "b", cover(window(2), 10_000, true)).await;
        assert!(muted().await);
        audio.write_muted(false).write().await.unwrap();
        put(&client, "b", cover(window(2), 10_000, true)).await;
        assert!(!muted().await);
        lift("b").await;

        // Muted before: stays muted after.
        audio.write_muted(true).write().await.unwrap();
        put(&client, "b", cover(window(2), 10_000, true)).await;
        lift("b").await;
        assert!(muted().await);
        audio.write_muted(false).write().await.unwrap();

        // The owner's link goes: the sink is unmuted.
        put(&client, "b", cover(window(2), 10_000, true)).await;
        assert!(muted().await);
        tv.drop_connection();
        soon("the mute lifts with the link", async {
            while !tv.overlay().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let client = connect(&tv).await.unwrap();
        assert!(!client.audio().read_muted().read().await.unwrap());
    });
}
