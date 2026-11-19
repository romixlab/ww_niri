//! The control API announced on the LAN over DNS-SD as `_wireweaver._tcp` (`ww_dns_sd`).
//!
//! One record per bound `lan:PORT` listener, at the LAN address only: the record follows the
//! address and goes with its listener. The in-process responder answers only on the interface
//! whose subnet holds the address, so nothing is announced on the tailnet or on bridges. A thread
//! of its own owns the responder: its calls block for a round trip (to mdns-sd's thread or to
//! Avahi), and the compositor's loop must not.
//!
//! Everything announced reaches every device on the LAN: the address, the port, the API and the
//! instance id. The id is random, kept in `~/.local/share/ww_niri/instance-id`.

use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context as _;
use directories::ProjectDirs;
use ww_dns_sd::{
    Advertiser, InstanceId, InstanceName, Responder, ResponderChoice, ServiceRecord, Txt,
    INSTANCE_NAME_LEN_MAX,
};

/// Request path announced; the server takes any.
const PATH: &str = "/ww";

/// Changes queued for the thread; with the queue full, the next check sends again.
const CHANGES_MAX: usize = 4;

/// How long the thread waits before trying again what failed: starting a responder or
/// announcing a record.
const RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// File holding the instance id, in ww_niri's data directory.
const ID_FILE: &str = "instance-id";

/// An id file is one short line; more than this is not one.
// spirit-level: allow(exact-units-quantity) the argument of Read::take, a plain u64 of bytes
const ID_FILE_BYTES_MAX: u64 = 64;

/// Random bytes in a new instance id: 16 hex digits.
const ID_RANDOM_LEN: usize = 8;

/// Name announced when neither `ww.kdl` nor the host has one.
const FALLBACK_NAME: &str = "ww_niri";

/// Keeps the records of the `lan:` listeners announced, through the thread.
#[derive(Default)]
pub struct Advertising {
    id: Id,
    /// What the thread was last sent.
    sent: Vec<ServiceRecord>,
    thread: Option<Thread>,
}

#[derive(Default)]
enum Id {
    #[default]
    Unread,
    Read(InstanceId),
    /// Reading or making it failed: nothing is announced until niri restarts.
    Failed,
}

struct Thread {
    changes: SyncSender<Vec<ServiceRecord>>,
    handle: JoinHandle<()>,
}

impl Advertising {
    /// Announces the API at `lan`, the addresses of the bound `lan:` listeners in `ww.kdl`'s
    /// order, under `name` (the host name when `None`); no addresses withdraw every record.
    /// Cheap when nothing changed.
    pub fn follow(&mut self, name: Option<&InstanceName>, lan: &[SocketAddr]) {
        if lan.is_empty() {
            self.send(Vec::new());
            return;
        }
        let Some(id) = self.id() else {
            return;
        };
        let name = name.cloned().unwrap_or_else(host_name);
        match records(&name, &id, lan) {
            Ok(records) => self.send(records),
            Err(err) => warn!("ww: error making the DNS-SD records: {err:?}"),
        }
    }

    fn id(&mut self) -> Option<InstanceId> {
        if let Id::Unread = self.id {
            self.id = match ProjectDirs::from("", "", "ww_niri")
                .context("no home directory")
                .and_then(|dirs| load_or_make_id(dirs.data_dir()))
            {
                Ok(id) => Id::Read(id),
                Err(err) => {
                    warn!("ww: not announcing on the LAN, no instance id: {err:?}");
                    Id::Failed
                }
            };
        }
        match &self.id {
            Id::Read(id) => Some(id.clone()),
            Id::Unread | Id::Failed => None,
        }
    }

    fn send(&mut self, records: Vec<ServiceRecord>) {
        if records == self.sent {
            return;
        }
        if self.thread.is_none() {
            if records.is_empty() {
                self.sent = records;
                return;
            }
            match spawn() {
                Ok(thread) => self.thread = Some(thread),
                Err(err) => {
                    warn!("ww: error starting the DNS-SD thread: {err:?}");
                    return;
                }
            }
        }
        let Some(thread) = &self.thread else {
            return;
        };
        match thread.changes.try_send(records.clone()) {
            Ok(()) => self.sent = records,
            Err(TrySendError::Full(_)) => debug!("ww: the DNS-SD thread is behind, sending later"),
            Err(TrySendError::Disconnected(_)) => {
                warn!("ww: the DNS-SD thread is gone, starting it again");
                self.thread = None;
                self.sent = Vec::new();
            }
        }
    }
}

impl Drop for Advertising {
    /// Withdraws every record before niri exits.
    fn drop(&mut self) {
        if let Some(Thread { changes, handle }) = self.thread.take() {
            drop(changes);
            if handle.join().is_err() {
                warn!("ww: the DNS-SD thread panicked");
            }
        }
    }
}

fn spawn() -> std::io::Result<Thread> {
    let (changes, rx) = mpsc::sync_channel(CHANGES_MAX);
    let handle = std::thread::Builder::new()
        .name("ww-dns-sd".to_owned())
        .spawn(move || run(rx, start_responder))?;
    Ok(Thread { changes, handle })
}

fn start_responder() -> Result<Box<dyn Responder>, ww_dns_sd::Error> {
    ww_dns_sd::start_responder(ResponderChoice::Auto)
}

/// The thread: applies each change, tries again what failed, and withdraws everything when the
/// sender goes.
fn run<F>(changes: Receiver<Vec<ServiceRecord>>, start: F)
where
    F: FnMut() -> Result<Box<dyn Responder>, ww_dns_sd::Error>,
{
    let mut announcing = Announcing::new(start);
    let mut wanted = Vec::new();
    loop {
        let next = if announcing.failed {
            changes.recv_timeout(RETRY_INTERVAL)
        } else {
            changes.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match next {
            Ok(records) => wanted = records,
            Err(RecvTimeoutError::Timeout) => (),
            Err(RecvTimeoutError::Disconnected) => break,
        }
        announcing.apply(&wanted);
    }
    announcing.apply(&[]);
}

/// One responder shared by the advertisers of every record.
#[derive(Clone)]
struct Shared(Arc<Mutex<Box<dyn Responder>>>);

impl Responder for Shared {
    fn publish(&mut self, record: &ServiceRecord) -> Result<(), ww_dns_sd::Error> {
        self.0
            .lock()
            .expect("a responder call panicked")
            .publish(record)
    }

    fn withdraw(&mut self, instance: &InstanceName) -> Result<(), ww_dns_sd::Error> {
        self.0
            .lock()
            .expect("a responder call panicked")
            .withdraw(instance)
    }
}

/// The thread's state: the responder, started while there is something to announce, and one
/// advertiser per record.
struct Announcing<F> {
    start: F,
    responder: Option<Shared>,
    advertisers: Vec<Advertiser<Shared>>,
    /// Something failed: the thread tries again after [`RETRY_INTERVAL`].
    failed: bool,
}

impl<F> Announcing<F>
where
    F: FnMut() -> Result<Box<dyn Responder>, ww_dns_sd::Error>,
{
    fn new(start: F) -> Self {
        Self {
            start,
            responder: None,
            advertisers: Vec::new(),
            failed: false,
        }
    }

    /// Announces `wanted` and withdraws the rest; none stops the responder.
    fn apply(&mut self, wanted: &[ServiceRecord]) {
        let was_failed = std::mem::take(&mut self.failed);
        // A record whose name, port or TXT changed goes before its successor comes.
        self.advertisers
            .retain(|adv| wanted.iter().any(|w| same_service(adv.record(), w)));
        if wanted.is_empty() {
            self.responder = None;
            return;
        }

        let responder = match &self.responder {
            Some(responder) => responder.clone(),
            None => match (self.start)() {
                Ok(responder) => {
                    let responder = Shared(Arc::new(Mutex::new(responder)));
                    self.responder = Some(responder.clone());
                    responder
                }
                Err(err) => {
                    if !was_failed {
                        warn!("ww: no DNS-SD responder, trying again: {err}");
                    }
                    self.failed = true;
                    return;
                }
            },
        };

        for record in wanted {
            let instance = record.instance();
            let addresses = record.addresses();
            match self
                .advertisers
                .iter_mut()
                .find(|adv| same_service(adv.record(), record))
            {
                Some(adv) => match adv.set_addresses(addresses.iter().copied()) {
                    Ok(true) => info!("ww: announcing {instance:?} at {addresses:?}"),
                    Ok(false) => (),
                    Err(err) => {
                        warn!("ww: error announcing {instance:?} at {addresses:?}: {err}");
                        self.failed = true;
                    }
                },
                None => match Advertiser::new(responder.clone(), record.clone()) {
                    Ok(adv) => {
                        info!(
                            "ww: announcing {instance:?} at {addresses:?} as {}",
                            ww_dns_sd::SERVICE_TYPE
                        );
                        self.advertisers.push(adv);
                    }
                    Err(err) => {
                        warn!("ww: error announcing {instance:?} at {addresses:?}: {err}");
                        self.failed = true;
                    }
                },
            }
        }
    }
}

/// Whether `a` and `b` announce the same service, maybe at other addresses.
fn same_service(a: &ServiceRecord, b: &ServiceRecord) -> bool {
    a.instance() == b.instance() && a.port() == b.port() && a.txt() == b.txt()
}

/// One record per address of `lan`: the first under `name` and `id`, each further one (another
/// port) with its port added to both, so names and ids stay unique on the LAN.
fn records(
    name: &InstanceName,
    id: &InstanceId,
    lan: &[SocketAddr],
) -> anyhow::Result<Vec<ServiceRecord>> {
    let mut records = Vec::with_capacity(lan.len());
    for (i, addr) in lan.iter().enumerate() {
        let port = NonZeroU16::new(addr.port())
            .with_context(|| format!("lan:{} has no fixed port to announce", addr.port()))?;
        let (name, id) = if i == 0 {
            (name.clone(), id.clone())
        } else {
            (
                fit_name(name.as_str(), &format!(" ({port})")),
                InstanceId::new(format!("{id}-{port}"))?,
            )
        };
        let txt = Txt::new(
            PATH,
            &ww_niri_api::WW_NIRI_FULL_GID,
            Some(ww_niri_api::WW_NIRI_WIRE_VERSION),
            id,
        )?;
        records.push(ServiceRecord::new(name, port, txt).with_addresses([addr.ip()])?);
    }
    Ok(records)
}

/// `base` cut to fit one DNS label with `suffix`, at a character boundary.
fn fit_name(base: &str, suffix: &str) -> InstanceName {
    let room = INSTANCE_NAME_LEN_MAX.saturating_sub(suffix.len());
    let mut end = base.len().min(room);
    while !base.is_char_boundary(end) {
        end -= 1;
    }
    let text = format!("{}{suffix}", &base[..end]);
    InstanceName::new(text).unwrap_or_else(|_| fallback_name())
}

/// The host name as the instance name.
fn host_name() -> InstanceName {
    match nix::unistd::gethostname() {
        Ok(name) => {
            let name = name.to_string_lossy();
            if name.is_empty() {
                fallback_name()
            } else {
                fit_name(&name, "")
            }
        }
        Err(err) => {
            debug!("ww: error reading the host name: {err:?}");
            fallback_name()
        }
    }
}

fn fallback_name() -> InstanceName {
    InstanceName::new(FALLBACK_NAME).expect("a valid instance name")
}

/// The instance id in `dir`, made and saved on first use.
fn load_or_make_id(dir: &Path) -> anyhow::Result<InstanceId> {
    let path = dir.join(ID_FILE);
    match std::fs::File::open(&path) {
        Ok(file) => {
            let mut text = String::new();
            file.take(ID_FILE_BYTES_MAX)
                .read_to_string(&mut text)
                .with_context(|| format!("error reading {path:?}"))?;
            InstanceId::new(text.trim()).with_context(|| format!("{path:?} holds no instance id"))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let id = random_id()?;
            std::fs::create_dir_all(dir).with_context(|| format!("error creating {dir:?}"))?;
            // Written aside and renamed, so a crash never leaves half an id.
            let partial = dir.join(format!("{ID_FILE}.partial"));
            std::fs::File::create(&partial)
                .and_then(|mut file| writeln!(file, "{id}").and_then(|()| file.sync_all()))
                .with_context(|| format!("error writing {partial:?}"))?;
            std::fs::rename(&partial, &path)
                .with_context(|| format!("error renaming {partial:?} to {path:?}"))?;
            info!("ww: new instance id {id} in {path:?}");
            Ok(id)
        }
        Err(err) => Err(err).with_context(|| format!("error opening {path:?}")),
    }
}

fn random_id() -> anyhow::Result<InstanceId> {
    let mut bytes = [0; ID_RANDOM_LEN];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .context("error reading /dev/urandom")?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(InstanceId::new(hex)?)
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    #[derive(Debug, PartialEq)]
    enum Call {
        Start,
        Publish(String, Vec<IpAddr>),
        Withdraw(String),
        Stop,
    }

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<Call>>>);

    impl Log {
        fn push(&self, call: Call) {
            self.0.lock().unwrap().push(call);
        }

        fn take(&self) -> Vec<Call> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    struct Fake(Log);

    impl Responder for Fake {
        fn publish(&mut self, record: &ServiceRecord) -> Result<(), ww_dns_sd::Error> {
            let name = record.instance().as_str().to_owned();
            self.0.push(Call::Publish(name, record.addresses().to_vec()));
            Ok(())
        }

        fn withdraw(&mut self, instance: &InstanceName) -> Result<(), ww_dns_sd::Error> {
            self.0.push(Call::Withdraw(instance.as_str().to_owned()));
            Ok(())
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.0.push(Call::Stop);
        }
    }

    fn starter(log: &Log) -> impl FnMut() -> Result<Box<dyn Responder>, ww_dns_sd::Error> {
        let log = log.clone();
        move || {
            log.push(Call::Start);
            Ok(Box::new(Fake(log.clone())))
        }
    }

    fn name(text: &str) -> InstanceName {
        InstanceName::new(text).unwrap()
    }

    fn id() -> InstanceId {
        InstanceId::new("0123abcd").unwrap()
    }

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn records_per_lan_listener() {
        let lan = [addr("192.168.1.95:7590"), addr("192.168.1.95:7591")];
        let both = records(&name("Living room TV"), &id(), &lan).unwrap();
        assert_eq!(both.len(), 2);

        let first = &both[0];
        assert_eq!(first.instance().as_str(), "Living room TV");
        assert_eq!(first.port().get(), 7590);
        assert_eq!(first.addresses(), [ip("192.168.1.95")]);
        let txt: Vec<(&str, String)> = first
            .txt()
            .entries()
            .map(|(k, v)| (k, v.to_owned()))
            .collect();
        let api = &ww_niri_api::WW_NIRI_FULL_GID;
        let v = &api.version;
        let want_api = format!("{}@{}.{}.{}", api.crate_id, v.major.0, v.minor.0, v.patch.0);
        assert_eq!(
            txt,
            [
                ("path", "/ww".to_owned()),
                ("api", want_api),
                ("wire", ww_niri_api::WW_NIRI_WIRE_VERSION.to_string()),
                ("id", "0123abcd".to_owned()),
            ]
        );

        let second = &both[1];
        assert_eq!(second.instance().as_str(), "Living room TV (7591)");
        assert_eq!(second.txt().id().as_str(), "0123abcd-7591");
        assert_eq!(second.port().get(), 7591);

        assert!(records(&name("TV"), &id(), &[addr("192.168.1.95:0")]).is_err());
    }

    #[test]
    fn long_names_are_cut_at_a_character() {
        let long = "ж".repeat(40);
        let fitted = fit_name(&long, " (7591)");
        assert!(fitted.as_str().len() <= INSTANCE_NAME_LEN_MAX);
        assert!(fitted.as_str().ends_with("ж (7591)"));
        assert_eq!(fit_name("tv", "").as_str(), "tv");
    }

    #[test]
    fn appears_follows_the_address_and_goes() {
        let log = Log::default();
        let mut announcing = Announcing::new(starter(&log));
        let lan = |text: &str| records(&name("TV"), &id(), &[addr(text)]).unwrap();

        announcing.apply(&lan("192.168.1.95:7590"));
        assert_eq!(
            log.take(),
            [
                Call::Start,
                Call::Publish("TV".to_owned(), vec![ip("192.168.1.95")])
            ]
        );

        // The same again: nothing sent.
        announcing.apply(&lan("192.168.1.95:7590"));
        assert_eq!(log.take(), []);

        // DHCP hands out another address: the same record, the new address.
        announcing.apply(&lan("192.168.1.96:7590"));
        assert_eq!(
            log.take(),
            [Call::Publish("TV".to_owned(), vec![ip("192.168.1.96")])]
        );

        // The listener goes: the record goes, and the responder stops.
        announcing.apply(&[]);
        assert_eq!(log.take(), [Call::Withdraw("TV".to_owned()), Call::Stop]);
        assert!(!announcing.failed);
    }

    #[test]
    fn a_new_name_replaces_the_record() {
        let log = Log::default();
        let mut announcing = Announcing::new(starter(&log));
        let lan = [addr("192.168.1.95:7590")];
        announcing.apply(&records(&name("TV"), &id(), &lan).unwrap());
        log.take();

        announcing.apply(&records(&name("Living room TV"), &id(), &lan).unwrap());
        assert_eq!(
            log.take(),
            [
                Call::Withdraw("TV".to_owned()),
                Call::Publish("Living room TV".to_owned(), vec![ip("192.168.1.95")])
            ]
        );
    }

    #[test]
    fn a_responder_that_fails_to_start_is_tried_again() {
        let log = Log::default();
        let mut fails = 1;
        let mut ok = starter(&log);
        let mut announcing = Announcing::new(move || {
            if fails > 0 {
                fails -= 1;
                return Err(ww_dns_sd::Error::ResponderGone);
            }
            ok()
        });
        let wanted = records(&name("TV"), &id(), &[addr("192.168.1.95:7590")]).unwrap();

        announcing.apply(&wanted);
        assert!(announcing.failed);
        assert_eq!(log.take(), []);

        announcing.apply(&wanted);
        assert!(!announcing.failed);
        assert_eq!(
            log.take(),
            [
                Call::Start,
                Call::Publish("TV".to_owned(), vec![ip("192.168.1.95")])
            ]
        );
    }

    #[test]
    fn the_thread_withdraws_when_the_sender_goes() {
        let log = Log::default();
        let (changes, rx) = mpsc::sync_channel(CHANGES_MAX);
        let start = starter(&log);
        let handle = std::thread::spawn(move || run(rx, start));
        let wanted = records(&name("TV"), &id(), &[addr("192.168.1.95:7590")]).unwrap();
        changes.send(wanted).unwrap();
        drop(changes);
        handle.join().unwrap();
        assert_eq!(
            log.take(),
            [
                Call::Start,
                Call::Publish("TV".to_owned(), vec![ip("192.168.1.95")]),
                Call::Withdraw("TV".to_owned()),
                Call::Stop,
            ]
        );
    }

    #[test]
    fn the_id_is_made_once_and_kept() {
        let dir = std::env::temp_dir().join(format!("ww-niri-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = load_or_make_id(&dir).unwrap();
        assert_eq!(first.as_str().len(), 2 * ID_RANDOM_LEN);
        assert_eq!(load_or_make_id(&dir).unwrap(), first);

        std::fs::write(dir.join(ID_FILE), "Not An Id\n").unwrap();
        assert!(load_or_make_id(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
