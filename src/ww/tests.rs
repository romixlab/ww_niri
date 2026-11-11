//! The server over a real WebSocket: a calloop loop on a thread serves one connection, a
//! generated client calls it.

use std::net::TcpListener;

use wire_weaver_client::{ClientConfig, Commander, WwClient};

use super::connection::serve;

struct NiriClient {
    cmd: Commander,
}

impl WwClient for NiriClient {
    fn default_config() -> ClientConfig {
        ClientConfig::new()
    }

    fn try_from_cmd(cmd: Commander) -> Result<Self, wire_weaver_client::Error> {
        Ok(Self { cmd })
    }
}

mod api_client {
    wire_weaver::ww_codegen!(ww_niri_api :: WwNiri for super::NiriClient, client = "std_client");
}

/// Serves the one connection that comes to `listener` on a calloop loop of its own, until told to
/// stop.
fn serve_one(listener: TcpListener) -> (calloop::channel::Sender<()>, std::thread::JoinHandle<()>) {
    let (stop_tx, stop_rx) = calloop::channel::channel::<()>();
    let thread = std::thread::spawn(move || {
        let mut event_loop = calloop::EventLoop::<bool>::try_new().unwrap();
        let handle = event_loop.handle();
        let (executor, scheduler) = calloop::futures::executor().unwrap();
        handle.insert_source(executor, |(), _, _| {}).unwrap();
        handle
            .insert_source(stop_rx, |_, _, stop| *stop = true)
            .unwrap();

        let (stream, peer) = listener.accept().unwrap();
        stream.set_nodelay(true).unwrap();
        let stream = handle.adapt_io(stream).unwrap();
        scheduler
            .schedule(serve(handle.clone(), stream, peer))
            .unwrap();

        let mut stop = false;
        while !stop {
            event_loop.dispatch(None, &mut stop).unwrap();
        }
    });
    (stop_tx, thread)
}

#[test]
fn info_over_websocket() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/ww", listener.local_addr().unwrap());
    let (stop, thread) = serve_one(listener);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let client = NiriClient::config(|c| c.websocket_url(url.clone()))
            .connect()
            .await
            .unwrap();
        let info = client.info().call().await.unwrap();
        assert_eq!(info.ww_niri_version, env!("CARGO_PKG_VERSION"));
        assert!(!info.niri_version.is_empty());
    });

    stop.send(()).unwrap();
    thread.join().unwrap();
}
