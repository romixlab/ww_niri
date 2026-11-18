//! The test knobs: errors on demand, latency, dropped and refused connections, `Resync`.

use std::time::{Duration, Instant};

use wire_weaver_client::observe::on_change;
use wire_weaver_client::CallError;
use ww_niri_api::{BrowserErrorOwned, EventOwned, Volume, WindowError, WindowId};
use ww_niri_fake::{ErrorKind, Fail, FailError};

use super::{connect, fake, run, soon};

#[test]
fn errors_on_demand() {
    let tv = fake();
    assert_eq!(
        tv.fail_next("windows.nope", Fail::Window(WindowError::NoSuchWindow)),
        Err(FailError::UnknownMethod("windows.nope".to_owned()))
    );
    assert_eq!(
        tv.fail_next("windows.show", Fail::Property("no")),
        Err(FailError::WrongKind {
            method: "windows.show",
            answers: ErrorKind::Window
        })
    );
    tv.fail_next("windows.show", Fail::Window(WindowError::NoSuchWindow))
        .unwrap();
    tv.fail_next(
        "browsers.start",
        Fail::Browser(BrowserErrorOwned::Spawn("no firefox".to_owned())),
    )
    .unwrap();
    tv.fail_next("audio.volume", Fail::Property("audio server gone"))
        .unwrap();
    run(async {
        let client = connect(&tv).await.unwrap();
        // Once each, then the fake acts again.
        let show = || client.windows().show(WindowId(1), None, None).call();
        assert!(matches!(
            show().await,
            Err(CallError::Device(WindowError::NoSuchWindow))
        ));
        show().await.unwrap();
        assert!(matches!(
            client.browsers(1).start(true).call().await,
            Err(CallError::Device(BrowserErrorOwned::Spawn(e))) if e == "no firefox"
        ));
        let refused = client.audio().read_volume().read().await.unwrap_err();
        assert!(
            refused.to_string().contains("audio server gone"),
            "{refused}"
        );
        assert_eq!(client.audio().read_volume().read().await.unwrap().get(), 40);
    });
    // A refused call is on the record too.
    let shown: Vec<String> = tv.calls().iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "windows.show(WindowId(1), None, None)",
            "windows.show(WindowId(1), None, None)",
            "browsers[1].start(true)",
        ]
    );
}

#[test]
fn latency_keeps_calls_in_flight() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        tv.set_latency(Duration::from_millis(50));
        let started = Instant::now();
        client.info().call().await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(50));
        tv.set_latency(Duration::ZERO);
    });
}

#[test]
fn dropped_and_refused_connections() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        client.info().call().await.unwrap();
        assert_eq!(tv.links_up(), 1);

        // Dropped: calls on the old connection fail, a new one comes up at once.
        tv.drop_connection();
        soon("the old connection fails", async {
            while client.info().call().await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let client = connect(&tv).await.unwrap();
        client.info().call().await.unwrap();
        assert_eq!(tv.links_up(), 2);

        // Unreachable: no connection until it is back.
        tv.set_reachable(false);
        soon("the connection fails", async {
            while client.info().call().await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(connect(&tv).await.is_err());
        tv.set_reachable(true);
        let client = soon("a connection once reachable", async {
            loop {
                if let Ok(client) = connect(&tv).await {
                    break client;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        client.info().call().await.unwrap();
    });
}

#[test]
fn resync_has_the_client_list_again() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        let mut events = client.events().call().await.unwrap();
        let audio = client.audio();
        let mut volumes = audio
            .observe_volume(on_change(Duration::ZERO))
            .subscribe()
            .await
            .unwrap();
        assert_eq!(
            soon("the volume", volumes.recv())
                .await
                .unwrap()
                .value
                .get(),
            40
        );
        audio
            .write_volume(Volume::try_new(41).unwrap())
            .write()
            .await
            .unwrap();
        soon("volume 41", async {
            while volumes.recv().await.unwrap().value.get() != 41 {}
        })
        .await;

        tv.resync();
        assert_eq!(
            soon("Resync", events.recv()).await.unwrap(),
            EventOwned::Resync
        );
    });
}
