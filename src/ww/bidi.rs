//! A small WebDriver BiDi client: JSON commands and events over a WebSocket, on the loop.
//!
//! [connect] gives a [Bidi] to send commands with and a [Reader] that must be polled for
//! commands to get their answers: [Reader::next_event] reads the socket, hands each answer to its
//! command and returns the events. A browser slot's task keeps polling it for the life of the
//! session.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;

use async_channel::Sender;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::{WebSocketReceiver, WebSocketSender};
use calloop::io::Async;
use calloop::LoopHandle;
use futures_util::lock::Mutex;
use futures_util::StreamExt as _;
use serde_json::{json, Value};

type Stream = Async<'static, TcpStream>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BidiError {
    /// The connection is gone.
    Closed,
    /// Connecting or sending failed.
    Io(String),
    /// The browser answered with an error.
    Remote { error: String, message: String },
}

impl std::fmt::Display for BidiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BidiError::Closed => write!(f, "connection closed"),
            BidiError::Io(err) => write!(f, "{err}"),
            BidiError::Remote { error, message } => write!(f, "{error}: {message}"),
        }
    }
}

/// An event from the browser: `browsingContext.load` and its params.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub method: String,
    pub params: Value,
}

/// Commands waiting for their answer, and whether the connection is gone.
#[derive(Default)]
struct Shared {
    pending: RefCell<HashMap<u64, Sender<Result<Value, BidiError>>>>,
    closed: Cell<bool>,
}

pub struct Bidi {
    tx: Mutex<WebSocketSender<Stream>>,
    next_id: Cell<u64>,
    shared: Rc<Shared>,
}

pub struct Reader {
    rx: WebSocketReceiver<Stream>,
    shared: Rc<Shared>,
}

/// Opens the WebSocket at `ws://ADDR/session` (no session yet: send `session.new`).
pub async fn connect<D: 'static>(
    handle: &LoopHandle<'static, D>,
    addr: SocketAddr,
) -> Result<(Bidi, Reader), BidiError> {
    let io = |err: &dyn std::fmt::Display| BidiError::Io(err.to_string());

    // Connecting to a port on this machine answers at once.
    let stream = TcpStream::connect(addr).map_err(|err| io(&err))?;
    stream.set_nodelay(true).map_err(|err| io(&err))?;
    let stream = handle.adapt_io(stream).map_err(|err| io(&err))?;
    let (ws, _) = async_tungstenite::client_async(format!("ws://{addr}/session"), stream)
        .await
        .map_err(|err| io(&err))?;

    let (tx, rx) = ws.split();
    let shared = Rc::new(Shared::default());
    let bidi = Bidi {
        tx: Mutex::new(tx),
        next_id: Cell::new(1),
        shared: shared.clone(),
    };
    Ok((bidi, Reader { rx, shared }))
}

impl Bidi {
    /// Sends a command and waits for its result; the [Reader] must be polled meanwhile.
    pub async fn command(&self, method: &str, params: Value) -> Result<Value, BidiError> {
        if self.shared.closed.get() {
            return Err(BidiError::Closed);
        }
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let (tx, rx) = async_channel::bounded(1);
        self.shared.pending.borrow_mut().insert(id, tx);

        let text = json!({ "id": id, "method": method, "params": params }).to_string();
        let sent = self.tx.lock().await.send(Message::text(text)).await;
        if let Err(err) = sent {
            self.shared.pending.borrow_mut().remove(&id);
            if self.shared.closed.get() {
                return Err(BidiError::Closed);
            }
            return Err(BidiError::Io(err.to_string()));
        }
        // The reader drops the sender when the connection ends.
        rx.recv().await.unwrap_or(Err(BidiError::Closed))
    }

    /// Closes the WebSocket; the browser's end sees the close frame.
    pub async fn close(&self) {
        let _ = self.tx.lock().await.close(None).await;
    }
}

impl Reader {
    /// The next event; `None` once the connection is closed. Answers to commands are handed over
    /// on the way.
    pub async fn next_event(&mut self) -> Option<Event> {
        loop {
            let text = match self.rx.next().await {
                Some(Ok(Message::Text(text))) => text,
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    // Commands waiting for an answer get Closed.
                    self.shared.closed.set(true);
                    self.shared.pending.borrow_mut().clear();
                    return None;
                }
                Some(Ok(_)) => continue,
            };
            let Ok(msg) = serde_json::from_str::<Value>(&text) else {
                warn!("ww: BiDi message that is not JSON: {text}");
                continue;
            };

            match msg["type"].as_str() {
                Some("event") => {
                    return Some(Event {
                        method: msg["method"].as_str().unwrap_or_default().to_owned(),
                        params: msg["params"].clone(),
                    });
                }
                Some(kind @ ("success" | "error")) => {
                    let id = msg["id"].as_u64();
                    let Some(tx) = id.and_then(|id| self.shared.pending.borrow_mut().remove(&id))
                    else {
                        // A command without an id, or one that gave up waiting.
                        debug!("ww: BiDi {kind} for no command: {text}");
                        continue;
                    };
                    let res = if kind == "success" {
                        Ok(msg["result"].clone())
                    } else {
                        Err(BidiError::Remote {
                            error: msg["error"].as_str().unwrap_or_default().to_owned(),
                            message: msg["message"].as_str().unwrap_or_default().to_owned(),
                        })
                    };
                    let _ = tx.try_send(res);
                }
                _ => debug!("ww: BiDi message of no known type: {text}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use async_tungstenite::tungstenite;

    use super::*;

    /// Answers `session.new`, sends an event, refuses `browsingContext.navigate`, then closes.
    fn fake_browser() -> (SocketAddr, std::thread::JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let mut seen = Vec::new();
            let recv = |ws: &mut tungstenite::WebSocket<_>| -> Value {
                let msg = ws.read().unwrap();
                serde_json::from_str(msg.to_text().unwrap()).unwrap()
            };

            let cmd = recv(&mut ws);
            let event = json!({"type": "event", "method": "browsingContext.load",
                "params": {"url": "https://example.org/"}});
            ws.send(Message::text(event.to_string())).unwrap();
            let answer = json!({"type": "success", "id": cmd["id"],
                "result": {"sessionId": "s1", "capabilities": {}}});
            ws.send(Message::text(answer.to_string())).unwrap();
            seen.push(cmd);

            let cmd = recv(&mut ws);
            let answer = json!({"type": "error", "id": cmd["id"],
                "error": "invalid argument", "message": "bad url"});
            ws.send(Message::text(answer.to_string())).unwrap();
            seen.push(cmd);

            ws.close(None).unwrap();
            while ws.read().is_ok() {}
            seen
        });
        (addr, thread)
    }

    #[test]
    fn commands_and_events() {
        let (addr, browser) = fake_browser();

        let mut event_loop = calloop::EventLoop::<bool>::try_new().unwrap();
        let handle = event_loop.handle();
        let (executor, scheduler) = calloop::futures::executor().unwrap();
        handle
            .insert_source(executor, |(), _, done| *done = true)
            .unwrap();

        let events = Rc::new(RefCell::new(Vec::new()));
        let task_events = events.clone();
        let task_scheduler = scheduler.clone();
        let task_handle = handle.clone();
        scheduler
            .schedule(async move {
                let (bidi, mut reader) = connect(&task_handle, addr).await.unwrap();
                let (closed_tx, closed_rx) = async_channel::bounded(1);
                task_scheduler
                    .schedule(async move {
                        while let Some(event) = reader.next_event().await {
                            task_events.borrow_mut().push(event);
                        }
                        let _ = closed_tx.send(()).await;
                    })
                    .unwrap();

                let session = bidi
                    .command("session.new", json!({"capabilities": {}}))
                    .await
                    .unwrap();
                assert_eq!(session["sessionId"], "s1");

                let err = bidi
                    .command("browsingContext.navigate", json!({"url": "x"}))
                    .await
                    .unwrap_err();
                assert_eq!(
                    err,
                    BidiError::Remote {
                        error: "invalid argument".to_owned(),
                        message: "bad url".to_owned()
                    }
                );

                // The browser closes: the reader ends, and commands answer Closed.
                closed_rx.recv().await.unwrap();
                assert_eq!(
                    bidi.command("session.end", json!({})).await,
                    Err(BidiError::Closed)
                );
            })
            .unwrap();

        let mut done = false;
        while !done {
            event_loop.dispatch(None, &mut done).unwrap();
        }

        let seen = browser.join().unwrap();
        let methods: Vec<_> = seen.iter().map(|cmd| cmd["method"].clone()).collect();
        assert_eq!(methods, ["session.new", "browsingContext.navigate"]);
        assert_eq!(
            *events.borrow(),
            [Event {
                method: "browsingContext.load".to_owned(),
                params: json!({"url": "https://example.org/"}),
            }]
        );
    }
}
