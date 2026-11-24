//! A program mounted at `ext[NAME]`, reached over the TV's own link: a second fake serving on a
//! Unix socket, its restart seen as the view's status going `Absent` and `Up` again.

use std::path::{Path, PathBuf};

use wire_weaver_client::{ClientConfig, Commander, WwClient};
use ww_niri_api::InfoOwned;
use ww_niri_client::{MountPresenceOwned, MountView};
use ww_niri_fake::{ClockMode, FakeTv, Options};

use super::{connect, fake, run, soon};

/// A generated client of the mounted program's API: here ww_niri's own, served by a fake.
struct Mounted {
    cmd: Commander,
}

impl WwClient for Mounted {
    fn default_config() -> ClientConfig {
        ClientConfig::new()
    }

    fn try_from_cmd(cmd: Commander) -> Result<Self, wire_weaver_client::Error> {
        Ok(Self { cmd })
    }
}

mod mounted_client {
    wire_weaver::ww_codegen!(ww_niri_api :: WwNiri for super::Mounted, client = "std_client");
}

/// A fake that serves on `socket` too, named `hostname`.
fn program(socket: &Path, hostname: &str) -> FakeTv {
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
    let dir = std::env::temp_dir().join(format!("ww_niri_client-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

async fn status_up(view: &MountView, up: bool) {
    let mut status = view.status();
    soon(
        "the view's status",
        status.wait_for(|p| matches!(p, MountPresenceOwned::Up { .. }) == up),
    )
    .await
    .unwrap();
}

#[test]
fn a_mounted_program_over_the_tv_link_and_its_restart() {
    let path = socket("restart.sock");
    let host = fake();
    let first = program(&path, "mounted-1");
    host.mount("tv", &path);
    run(async {
        let tv = connect(&host).await;
        let line = Mounted::client_line();
        assert!(tv.ext_view("nope", &line).await.unwrap().is_none());
        let view = tv.ext_view("tv", &line).await.unwrap().expect("ext[tv]");
        status_up(&view, true).await;
        let mounted: Mounted = view.client(&line).unwrap();
        assert_eq!(mounted.info().call().await.unwrap().hostname, "mounted-1");

        drop(first);
        status_up(&view, false).await;
        assert!(mounted.info().call().await.is_err(), "nothing is mounted");
        assert_eq!(tv.info().await.unwrap().hostname, "fake-tv");
        let _second = program(&path, "mounted-2");
        status_up(&view, true).await;
        assert_eq!(mounted.info().call().await.unwrap().hostname, "mounted-2");
    });
}

#[test]
fn a_mount_name_past_64_bytes_is_refused() {
    let host = fake();
    run(async {
        let tv = connect(&host).await;
        let line = Mounted::client_line();
        assert!(tv.ext_view(&"x".repeat(65), &line).await.is_err());
    });
}
