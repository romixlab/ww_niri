//! Links of several clients at once: what a client held ends with its link, and the listeners
//! move to another path while open links go on.

use exact_units::Fixed;
use ww_niri_api::{Button, WindowId};
use ww_niri_fake::{Cover, InputEntry, ReleasedBy};

use super::covers::card;
use super::{connect, fake, run, soon};

fn windows_covered(covers: &[Cover]) -> Vec<WindowId> {
    covers.iter().map(|c| c.window).collect()
}

#[test]
fn a_link_that_ends_releases_only_what_its_client_held() {
    let tv = fake();
    run(async {
        let a = connect(&tv).await.unwrap();
        let b = connect(&tv).await.unwrap();
        (a.input().keyboard().key_down("shift".to_owned(), None))
            .call()
            .await
            .unwrap();
        (b.input().pointer().press(Button::Left, None))
            .call()
            .await
            .unwrap();
        let cover = |client: &super::NiriClient, id| {
            client
                .windows()
                .cover(WindowId(id), card(), Fixed::new(10_000), false)
        };
        cover(&a, 1).call().await.unwrap();
        cover(&b, 2).call().await.unwrap();
        tv.take_input_log();

        drop(a);
        soon("the key comes up with its link", async {
            while windows_covered(&tv.covers()) != [WindowId(2)] {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(
            tv.take_input_log(),
            [InputEntry::KeyUp {
                key: "shift".to_owned(),
                by: ReleasedBy::LinkDown
            }]
        );

        // The other client's link goes on, with what it holds.
        b.info().call().await.unwrap();
        drop(b);
        soon("the button comes up with the last link", async {
            while !tv.covers().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(
            tv.take_input_log(),
            [InputEntry::Release {
                button: Button::Left,
                by: ReleasedBy::LinkDown
            }]
        );
    });
}

#[test]
fn a_rebind_keeps_open_links_and_moves_new_ones() {
    let mut tv = fake();
    run(async {
        let old = connect(&tv).await.unwrap();
        let old_path = tv.path().to_owned();
        let new_path = tv.rebind().to_owned();
        assert_ne!(new_path, old_path);

        old.info().call().await.unwrap();
        let at_old = super::NiriClient::config(|c| {
            c.in_process_path(old_path.clone())
                .resolver(super::resolver())
        })
        .connect()
        .await;
        assert!(at_old.is_err(), "nobody listens at the old path");

        let new = connect(&tv).await.unwrap();
        new.info().call().await.unwrap();
        old.info().call().await.unwrap();
        assert_eq!(tv.links_up(), 2);
    });
}
