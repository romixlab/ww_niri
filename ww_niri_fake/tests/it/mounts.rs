//! The `ext` mount slots: a second fake TV serving on a Unix socket is mounted at `ext["tv"]`,
//! a client reaches it through the first one's link, and a restart of the mounted one shows as
//! its presence going `Absent` and `Up` again, the first TV untouched.

use std::path::{Path, PathBuf};
use std::time::Duration;

use wire_weaver::prelude::MaxLen;
use wire_weaver::ww_api::mount::{MountNameOwned, MountPresenceOwned};
use wire_weaver_client::MountView;
use ww_niri_api::InfoOwned;
use ww_niri_fake::{ClockMode, FakeTv, Options};

use super::{connect, run, soon, NiriClient};

/// A fake serving on `socket` too, named `hostname`.
fn mounted(socket: &Path, hostname: &str) -> FakeTv {
    FakeTv::start(Options {
        clock: ClockMode::Manual,
        socket: Some(socket.to_owned()),
        info: InfoOwned {
            niri_version: "fake".to_owned(),
            ww_niri_version: "fake".to_owned(),
            hostname: hostname.to_owned(),
        },
        ..Options::default()
    })
}

fn socket(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ww_niri_fake-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn name(text: &str) -> MountNameOwned {
    MountNameOwned(MaxLen::new(text.to_owned()).unwrap())
}

fn is_up(presence: &MountPresenceOwned) -> bool {
    matches!(presence, MountPresenceOwned::Up { .. })
}

/// Waits until the view's status is up (`true`) or absent.
async fn view_up(view: &MountView, up: bool) {
    let mut status = view.status();
    soon("the view's status", status.wait_for(|p| is_up(p) == up))
        .await
        .unwrap();
}

#[test]
fn a_mounted_tv_through_the_link_and_its_restart() {
    let path = socket("restart.sock");
    let host = FakeTv::start(Options {
        clock: ClockMode::Manual,
        ..Options::default()
    });
    let first = mounted(&path, "mounted-1");
    host.mount("tv", &path);
    run(async {
        let client = connect(&host).await.unwrap();
        let slot = soon("ext[\"tv\"]", client.ext_by(&name("tv")))
            .await
            .unwrap()
            .expect("a slot named tv");
        let line = NiriClient::client_line();
        let view = slot.view_for(&line).await.unwrap();
        view_up(&view, true).await;
        let tv: NiriClient = view.client(&line).unwrap();
        assert_eq!(tv.info().call().await.unwrap().hostname, "mounted-1");
        assert_eq!(client.info().call().await.unwrap().hostname, "fake-tv");

        // the mounted program restarts: absent, then up again at the same slot
        drop(first);
        view_up(&view, false).await;
        assert!(tv.info().call().await.is_err(), "nothing is mounted");
        assert_eq!(client.info().call().await.unwrap().hostname, "fake-tv");
        let _second = mounted(&path, "mounted-2");
        view_up(&view, true).await;
        assert_eq!(tv.info().call().await.unwrap().hostname, "mounted-2");
        assert!(is_up(&slot.presence().read().await.unwrap()));
    });
}

#[test]
fn slots_come_and_go_by_name() {
    let path = socket("names.sock");
    let host = FakeTv::start(Options {
        clock: ClockMode::Manual,
        ..Options::default()
    });
    run(async {
        let client = connect(&host).await.unwrap();
        assert!(client.ext_by(&name("tv")).await.unwrap().is_none());
        host.mount("tv", &path);
        let index = soon("ext[\"tv\"]", async {
            loop {
                if let Ok(Some(index)) = client.ext_index_of(&name("tv")).await {
                    return index;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let presence = client.ext(index).unwrap().presence().read().await.unwrap();
        assert!(!is_up(&presence), "nobody serves on the socket");
        assert!(host.unmount("tv"));
        assert!(!host.unmount("tv"));
        assert!(host.mount_presence("tv").is_none());
    });
}
