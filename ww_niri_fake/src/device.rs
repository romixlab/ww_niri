//! The device thread: a `ww_device::Server` on the in-process transport, serving one client at a
//! time, re-registered after a dropped connection; and the motion path, the same API on a
//! simulated UDP device, for a client's pointer motion.

use std::sync::Arc;

use wire_weaver_client::in_process::{self, TokioClock};
use ww_device::udp::{udp_server, UdpBuffers, UdpConnection};
use ww_device::LinkEvent;
use ww_niri_api::EventOwned;

use crate::model::{Model, Out};
use crate::server::{api_impl, Server};
use crate::Shared;

/// Longest message either way: window and player lists fit.
pub(crate) const MESSAGE_LEN_MAX: usize = 32 * 1024;

/// Why serving one registration ended.
enum End {
    /// The test dropped the connection or made the TV unreachable.
    Dropped,
    /// The fake is dropped.
    Stop,
}

/// The link's side of the API, the same on both paths.
fn link_config() -> ww_device::LinkConfig<'static> {
    ww_device::LinkConfig::new(
        ww_niri_api::WW_NIRI_FULL_GID,
        ww_niri_api::WW_NIRI_WIRE_VERSION,
        api_impl::api_hash(),
        api_impl::image_facts(),
        ww_client_server::COMPACT_VERSION,
    )
}

/// Waits until the TV is reachable; `false` when the fake is dropped first. `wake` is the
/// path's own notify.
async fn reachable(shared: &Shared, wake: &tokio::sync::Notify) -> bool {
    loop {
        let control = shared.control();
        if control.stop {
            return false;
        }
        if control.reachable {
            return true;
        }
        // Unreachable: nothing is registered, so a client finds no device.
        wake.notified().await;
    }
}

/// Serves `path` until the fake is dropped.
pub(crate) async fn run(shared: Arc<Shared>, path: String) {
    loop {
        if !reachable(&shared, &shared.wake).await {
            return;
        }
        shared.update_control(|c| c.drop_link = false);
        let (tx, rx) = in_process::device(&path, MESSAGE_LEN_MAX);
        match serve(&shared, tx, rx).await {
            End::Stop => return,
            End::Dropped => shared.with(Model::release_all),
        }
    }
}

async fn serve(shared: &Arc<Shared>, tx: in_process::DeviceTx, rx: in_process::DeviceRx) -> End {
    let mut scratch = vec![0u8; MESSAGE_LEN_MAX];
    let mut event_scratch = vec![0u8; MESSAGE_LEN_MAX];
    let mut observe_buf = vec![0u8; MESSAGE_LEN_MAX];
    let mut server = ww_device::Server::new(
        link_config(),
        tx,
        rx,
        TokioClock::new(),
        &mut scratch,
        &mut event_scratch,
    );
    let mut backend = Server::new(shared.clone());

    loop {
        let wake = (backend.observe)
            .next_wake(server.now().as_micros())
            .map(ww_device::Instant::from_micros);
        let deadline = shared.with(|m| m.next_deadline());
        tokio::select! {
            ready = server.wait_until(wake) => {
                match server.handle(ready, &mut backend).await {
                    Some(LinkEvent::Up) => shared.update_control(|c| c.links_up += 1),
                    Some(LinkEvent::Down(_)) => {
                        // The next client starts afresh; nothing this one held stays down.
                        backend = Server::new(shared.clone());
                        shared.with(Model::release_all);
                    }
                    None => {}
                }
            }
            () = shared.wake.notified() => {}
            () = shared.clock.until(deadline) => {}
        }

        let control = shared.control();
        if control.stop {
            return End::Stop;
        }
        if control.drop_link || !control.reachable {
            return End::Dropped;
        }

        let (outs, behind) = shared.with(Model::drain);
        let mut events = Vec::new();
        if behind {
            events.push(EventOwned::Resync);
            backend.mark_all();
        }
        for out in outs {
            match out {
                Out::Event(event) => events.push(event),
                Out::Mark(mark) => backend.mark(mark),
            }
        }
        if !server.is_up() {
            continue;
        }
        if backend.events_open {
            for event in &events {
                // A full queue drops the event: the client lists again after the Resync it
                // gets when the link catches up.
                if api_impl::stream_data_ser()
                    .events_send(event, &mut server.sink())
                    .await
                    .is_err()
                {
                    shared.change(Model::resync);
                    break;
                }
            }
        }
        let now = server.now().as_micros();
        // An observed value that can't be sent is sent with the next poll.
        let _sent = backend
            .poll_properties(&mut server.sink(), (), now, &mut observe_buf)
            .await;
    }
}

/// Serves the motion path until the fake is dropped: one host at a time, a new one takes it
/// over, as a UDP port with one slot. Its calls and writes act on the model; the model's events
/// and observed changes go to the client of the main path only.
pub(crate) async fn run_motion(shared: Arc<Shared>, path: String) {
    loop {
        if !reachable(&shared, &shared.wake_motion).await {
            return;
        }
        let connection = UdpConnection::new(in_process::udp_device(&path));
        if let End::Stop = serve_motion(&shared, &connection).await {
            return;
        }
    }
}

async fn serve_motion(
    shared: &Arc<Shared>,
    connection: &UdpConnection<in_process::UdpPort>,
) -> End {
    let mut buffers = Box::new(UdpBuffers::<MESSAGE_LEN_MAX>::new());
    let mut server = udp_server(
        link_config(),
        connection,
        0,
        TokioClock::new(),
        &mut buffers,
    );
    let mut backend = Server::new(shared.clone());
    let drops = shared.control().drops;
    loop {
        tokio::select! {
            ready = server.wait() => {
                if let Some(LinkEvent::Down(_)) = server.handle(ready, &mut backend).await {
                    backend = Server::new(shared.clone());
                }
            }
            () = shared.wake_motion.notified() => {}
        }
        let control = shared.control();
        if control.stop {
            return End::Stop;
        }
        if control.drops != drops || !control.reachable {
            return End::Dropped;
        }
    }
}
