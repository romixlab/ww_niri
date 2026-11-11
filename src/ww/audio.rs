//! System volume and mute: the default sink of the PulseAudio server (PipeWire's `pipewire-pulse`
//! on the TV box), over the PulseAudio protocol on the loop.
//!
//! A task connects to the server, subscribes to sink and server events and keeps the default
//! sink's state; every change, made here or elsewhere (a volume key, `wpctl`), tells the watchers.
//! When the server goes away the task connects again every few seconds. The `pulseaudio` crate
//! gives the messages; the socket is the loop's.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::{self, Cursor};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::pin::pin;
use std::rc::Rc;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use calloop::futures::Scheduler;
use calloop::io::Async;
use futures_util::future::{select, Either};
use futures_util::io::{AsyncReadExt as _, AsyncWriteExt as _, ReadHalf, WriteHalf};
use futures_util::lock::Mutex;
use pulseaudio::protocol::{
    self, ChannelVolume, Command, CommandReply, SubscriptionEventFacility, SubscriptionMask,
};

use super::browsers::LoopIo;

/// How long to wait before connecting again after the server went away or refused.
const RETRY: Duration = Duration::from_secs(5);
/// Longest message accepted from the server; sink info is a few KB.
const MAX_MESSAGE: u32 = 1 << 20;
/// 100 % on the protocol's volume scale.
const NORM: u32 = 0x10000;
/// The API's top volume, in percent.
const MAX_PERCENT: u16 = 150;

pub type Stream = Async<'static, UnixStream>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    /// Not connected to the server, or it has no default sink.
    NoSink,
    /// The connection is gone.
    Closed,
    Io(String),
    /// The server refused the command.
    Server(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::NoSink => write!(f, "no audio server or no default sink"),
            AudioError::Closed => write!(f, "connection closed"),
            AudioError::Io(err) => write!(f, "{err}"),
            AudioError::Server(err) => write!(f, "server error: {err}"),
        }
    }
}

impl From<io::Error> for AudioError {
    fn from(err: io::Error) -> Self {
        AudioError::Io(err.to_string())
    }
}

impl From<protocol::ProtocolError> for AudioError {
    fn from(err: protocol::ProtocolError) -> Self {
        match err {
            protocol::ProtocolError::ServerError(err) => AudioError::Server(format!("{err:?}")),
            err => AudioError::Io(err.to_string()),
        }
    }
}

/// Where the server is and the cookie to show it.
#[derive(Debug, Clone)]
pub struct Target {
    pub socket: PathBuf,
    pub cookie: Vec<u8>,
}

impl Target {
    /// As libpulse finds them: `PULSE_SERVER`, `PULSE_RUNTIME_PATH`, `XDG_RUNTIME_DIR`, and
    /// `PULSE_COOKIE` or `~/.config/pulse/cookie`. PipeWire ignores the cookie.
    pub fn from_env() -> Option<Self> {
        let socket = pulseaudio::socket_path_from_env()?;
        let cookie = pulseaudio::cookie_path_from_env()
            .and_then(|path| std::fs::read(path).ok())
            .unwrap_or_default();
        Some(Self { socket, cookie })
    }
}

/// The default sink as last read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkState {
    pub name: String,
    pub description: String,
    /// The loudest channel, in percent, up to [MAX_PERCENT].
    pub volume: u16,
    pub muted: bool,
    channels: u8,
}

/// Commands waiting for their reply (the whole message), and whether the connection is gone.
#[derive(Default)]
struct Shared {
    pending: RefCell<HashMap<u32, Sender<Vec<u8>>>>,
    closed: Cell<bool>,
}

/// Sends commands; a [Reader] must be polled for the replies.
pub struct Client {
    tx: Mutex<WriteHalf<Stream>>,
    version: u16,
    next_seq: Cell<u32>,
    shared: Rc<Shared>,
}

pub struct Reader {
    rx: ReadHalf<Stream>,
    version: u16,
    shared: Rc<Shared>,
}

/// Authenticates over `stream` and names this client; then splits it into a [Client] and its
/// [Reader].
pub async fn connect(mut stream: Stream, cookie: &[u8]) -> Result<(Client, Reader), AudioError> {
    let auth = protocol::AuthParams {
        version: protocol::MAX_VERSION,
        supports_shm: false,
        supports_memfd: false,
        cookie: cookie.to_owned(),
    };
    send(&mut stream, 0, &Command::Auth(auth), protocol::MAX_VERSION).await?;
    let message = read_message(&mut stream).await?;
    let (_, reply): (_, protocol::AuthReply) =
        protocol::read_reply_message(&mut Cursor::new(&message), protocol::MAX_VERSION)?;
    let version = reply.version.min(protocol::MAX_VERSION);

    let mut props = protocol::Props::new();
    props.set(protocol::Prop::ApplicationName, c"niri ww");
    props.set(
        protocol::Prop::ApplicationProcessId,
        CString::new(std::process::id().to_string()).unwrap(),
    );
    send(&mut stream, 1, &Command::SetClientName(props), version).await?;
    let message = read_message(&mut stream).await?;
    let _: (_, protocol::SetClientNameReply) =
        protocol::read_reply_message(&mut Cursor::new(&message), version)?;

    let (rx, tx) = stream.split();
    let shared = Rc::new(Shared::default());
    let client = Client {
        tx: Mutex::new(tx),
        version,
        next_seq: Cell::new(2),
        shared: shared.clone(),
    };
    Ok((
        client,
        Reader {
            rx,
            version,
            shared,
        },
    ))
}

async fn send(
    w: &mut (impl futures_util::AsyncWrite + Unpin),
    seq: u32,
    command: &Command,
    version: u16,
) -> Result<(), AudioError> {
    let mut buf = Vec::new();
    protocol::write_command_message(&mut buf, seq, command, version)?;
    w.write_all(&buf).await?;
    Ok(())
}

/// One message: the descriptor and its payload.
async fn read_message(r: &mut (impl futures_util::AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let mut message = vec![0; protocol::DESCRIPTOR_SIZE];
    r.read_exact(&mut message).await?;
    let desc = protocol::read_descriptor(&mut Cursor::new(&message)).map_err(io::Error::other)?;
    if desc.length > MAX_MESSAGE {
        return Err(io::Error::other(format!(
            "message of {} bytes from the server",
            desc.length
        )));
    }
    message.resize(protocol::DESCRIPTOR_SIZE + desc.length as usize, 0);
    r.read_exact(&mut message[protocol::DESCRIPTOR_SIZE..])
        .await?;
    Ok(message)
}

impl Client {
    /// Sends a command and waits for its reply message.
    async fn roundtrip(&self, command: Command) -> Result<Vec<u8>, AudioError> {
        if self.shared.closed.get() {
            return Err(AudioError::Closed);
        }
        let seq = self.next_seq.get();
        self.next_seq.set(seq.wrapping_add(1));
        let (tx, rx) = async_channel::bounded(1);
        self.shared.pending.borrow_mut().insert(seq, tx);

        let sent = send(&mut *self.tx.lock().await, seq, &command, self.version).await;
        if let Err(err) = sent {
            self.shared.pending.borrow_mut().remove(&seq);
            return Err(err);
        }
        // The reader drops the sender when the connection ends.
        rx.recv().await.map_err(|_| AudioError::Closed)
    }

    pub async fn request<T: CommandReply>(&self, command: Command) -> Result<T, AudioError> {
        let message = self.roundtrip(command).await?;
        let (_, reply) = protocol::read_reply_message(&mut Cursor::new(&message), self.version)?;
        Ok(reply)
    }

    /// A command whose reply carries nothing.
    pub async fn ack(&self, command: Command) -> Result<(), AudioError> {
        let message = self.roundtrip(command).await?;
        match protocol::read_command_message(&mut Cursor::new(&message), self.version)? {
            (_, Command::Error(err)) => Err(AudioError::Server(format!("{err:?}"))),
            _ => Ok(()),
        }
    }
}

impl Reader {
    /// The next subscription event's facility; `None` once the connection is closed. Replies
    /// are handed to their commands on the way.
    pub async fn next_event(&mut self) -> Option<SubscriptionEventFacility> {
        loop {
            let message = match read_message(&mut self.rx).await {
                Ok(message) => message,
                Err(err) => {
                    debug!("ww: audio server connection ended: {err}");
                    // Commands waiting for a reply get Closed.
                    self.shared.closed.set(true);
                    self.shared.pending.borrow_mut().clear();
                    return None;
                }
            };
            let mut cursor = Cursor::new(&message);
            let desc = protocol::read_descriptor(&mut cursor).ok()?;
            if desc.channel != u32::MAX {
                // Stream data; this client has no streams.
                continue;
            }
            let (seq, command) = match Command::read_tag_prefixed(&mut cursor, self.version) {
                Ok(read) => read,
                Err(err) => {
                    debug!("ww: unreadable message from the audio server: {err}");
                    continue;
                }
            };
            match command {
                Command::Reply | Command::Error(_) => {
                    if let Some(tx) = self.shared.pending.borrow_mut().remove(&seq) {
                        let _ = tx.try_send(message);
                    }
                }
                Command::SubscribeEvent(event) => return Some(event.event_facility),
                _ => (),
            }
        }
    }
}

pub struct Audio {
    client: RefCell<Option<Rc<Client>>>,
    sink: RefCell<Option<SinkState>>,
    watchers: RefCell<Vec<Sender<()>>>,
}

impl Audio {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            client: RefCell::new(None),
            sink: RefCell::new(None),
            watchers: RefCell::new(Vec::new()),
        })
    }

    /// Follows the server at `target` as a task on `scheduler`, for good.
    pub fn follow(self: &Rc<Self>, scheduler: &Scheduler<()>, io: LoopIo, target: Target) {
        let this = self.clone();
        let res = scheduler.schedule(async move {
            let mut warned = false;
            loop {
                let res = this.session(&io, &target).await;
                this.client.replace(None);
                this.update(None);
                match res {
                    // Only the first failure in a row is worth a warning.
                    Err(err) if !warned => {
                        warn!("ww: audio server at {:?}: {err}", target.socket);
                        warned = true;
                    }
                    Err(err) => debug!("ww: audio server at {:?}: {err}", target.socket),
                    Ok(()) => warned = false,
                }
                io.sleep(RETRY).await;
            }
        });
        if let Err(err) = res {
            warn!("ww: error scheduling the audio task: {err:?}");
        }
    }

    /// One connection, until it ends; `Ok` when it ended after it was up.
    async fn session(&self, io: &LoopIo, target: &Target) -> Result<(), AudioError> {
        let stream = io.unix(UnixStream::connect(&target.socket)?)?;
        let (client, mut reader) = connect(stream, &target.cookie).await?;
        let client = Rc::new(client);

        let (events_tx, events_rx) = async_channel::unbounded();
        let read = pin!(async move {
            while let Some(facility) = reader.next_event().await {
                let _ = events_tx.try_send(facility);
            }
        });
        let work = pin!(async {
            client
                .ack(Command::Subscribe(
                    SubscriptionMask::SINK | SubscriptionMask::SERVER,
                ))
                .await?;
            self.refresh(&client).await?;
            self.client.replace(Some(client.clone()));
            info!("ww: following the audio server at {:?}", target.socket);
            while let Ok(facility) = events_rx.recv().await {
                // One read for a burst of events (a volume slider sends many).
                let mut relevant = is_relevant(facility);
                while let Ok(facility) = events_rx.try_recv() {
                    relevant |= is_relevant(facility);
                }
                if relevant {
                    self.refresh(&client).await?;
                }
            }
            Ok::<_, AudioError>(())
        });
        match select(read, work).await {
            Either::Left(((), _)) => Ok(()),
            Either::Right((res, _)) => res,
        }
    }

    /// Reads the default sink again.
    async fn refresh(&self, client: &Client) -> Result<(), AudioError> {
        let server: protocol::ServerInfo = client.request(Command::GetServerInfo).await?;
        let Some(name) = server.default_sink_name else {
            self.update(None);
            return Ok(());
        };
        let info: Result<protocol::SinkInfo, _> = client
            .request(Command::GetSinkInfo(protocol::GetSinkInfo {
                index: None,
                name: Some(name),
            }))
            .await;
        let info = match info {
            Ok(info) => info,
            // The default changed meanwhile; its event brings another refresh.
            Err(AudioError::Server(_)) => {
                self.update(None);
                return Ok(());
            }
            Err(err) => return Err(err),
        };
        self.update(Some(sink_state(&info)));
        Ok(())
    }

    fn update(&self, sink: Option<SinkState>) {
        if *self.sink.borrow() == sink {
            return;
        }
        debug!("ww: default sink is {sink:?}");
        self.sink.replace(sink);
        self.watchers
            .borrow_mut()
            .retain(|tx| !matches!(tx.try_send(()), Err(async_channel::TrySendError::Closed(_))));
    }

    /// Gets a message after each change of the default sink (several changes may give one).
    pub fn watch(&self) -> Receiver<()> {
        let (tx, rx) = async_channel::bounded(1);
        self.watchers.borrow_mut().push(tx);
        rx
    }

    pub fn sink(&self) -> Option<SinkState> {
        self.sink.borrow().clone()
    }

    fn target(&self) -> Result<(Rc<Client>, SinkState), AudioError> {
        let client = self.client.borrow().clone().ok_or(AudioError::NoSink)?;
        let sink = self.sink().ok_or(AudioError::NoSink)?;
        Ok((client, sink))
    }

    /// Sets every channel of the default sink to `percent`.
    pub async fn set_volume(&self, percent: u16) -> Result<(), AudioError> {
        let (client, sink) = self.target()?;
        let raw = u32::from(percent.min(MAX_PERCENT)) * NORM / 100;
        let mut volume = ChannelVolume::empty();
        for _ in 0..sink.channels.max(1) {
            volume.push(protocol::Volume::from_u32_clamped(raw));
        }
        client
            .ack(Command::SetSinkVolume(protocol::SetDeviceVolumeParams {
                device_index: None,
                device_name: Some(c_name(&sink.name)?),
                volume,
            }))
            .await
    }

    pub async fn set_muted(&self, muted: bool) -> Result<(), AudioError> {
        let (client, sink) = self.target()?;
        client
            .ack(Command::SetSinkMute(protocol::SetDeviceMuteParams {
                device_index: None,
                device_name: Some(c_name(&sink.name)?),
                mute: muted,
            }))
            .await
    }
}

fn c_name(name: &str) -> Result<CString, AudioError> {
    CString::new(name).map_err(|err| AudioError::Io(err.to_string()))
}

fn is_relevant(facility: SubscriptionEventFacility) -> bool {
    matches!(
        facility,
        SubscriptionEventFacility::Sink | SubscriptionEventFacility::Server
    )
}

fn sink_state(info: &protocol::SinkInfo) -> SinkState {
    let loudest = info
        .cvolume
        .channels()
        .iter()
        .map(|v| v.as_u32())
        .max()
        .unwrap_or(0);
    let percent = (u64::from(loudest) * 100 + u64::from(NORM / 2)) / u64::from(NORM);
    SinkState {
        name: info.name.to_string_lossy().into_owned(),
        description: info
            .description
            .as_ref()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default(),
        volume: percent.min(u64::from(MAX_PERCENT)) as u16,
        muted: info.muted,
        channels: info.cvolume.channels().len() as u8,
    }
}
