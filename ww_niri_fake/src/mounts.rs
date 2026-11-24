//! The `ext` mount slots, as ww_niri has them from the `mount` lines of ww.kdl: each a name and
//! the Unix socket of the program mounted there, forwarded by `wire_weaver_server::mount`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wire_weaver::prelude::UNib32;
use wire_weaver::shrink_wrap::DeserializeShrinkWrapOwned;
use wire_weaver::ww_api::mount::MountPresenceOwned;
use wire_weaver_server::mount::{LocalSocket, MountOptions, MountRoutes, Mounts};
use ww_client_server::ResourceIndex;

use crate::lock;
use crate::server::api_impl;

/// Id of `ext` in the root of `ww_niri_api::WwNiri`.
const EXT_ID: u32 = 10;
/// Id of a slot's `api` below its element.
const API_ID: u32 = 1;
/// The longest slot name, as `ww_api::mount::MountName` takes it.
pub const MOUNT_NAME_LEN_MAX: usize = 64;
/// How soon a slot's control link tries again after its program went away, as on ww_niri:
/// shorter, so tests see a restarted program soon.
const RETRY_AFTER: Duration = Duration::from_millis(100);

/// The slots by name, and their mount table.
pub(crate) struct Ext {
    pub(crate) mounts: Mounts<LocalSocket>,
    /// (index, name) of each slot, in the order mounted; and the next new index.
    slots: Mutex<(Vec<(u32, String)>, u32)>,
}

impl Ext {
    pub(crate) fn new() -> Self {
        Self {
            mounts: Mounts::new(Arc::new(own_api())),
            slots: Mutex::new((Vec::new(), 0)),
        }
    }

    /// Mounts the program on `socket` at `ext[name]`; a name mounted already keeps its index.
    pub(crate) fn mount(&self, name: &str, socket: PathBuf) {
        let index = {
            let mut slots = lock(&self.slots);
            match slots.0.iter().find(|(_, n)| n == name) {
                Some((index, _)) => *index,
                None => {
                    let index = slots.1;
                    slots.1 += 1;
                    slots.0.push((index, name.to_owned()));
                    index
                }
            }
        };
        let options = MountOptions {
            retry_after: RETRY_AFTER,
            ..MountOptions::default()
        };
        (self.mounts).mount(&api_path(index), LocalSocket::new(socket), options);
    }

    /// Takes `ext[name]` away; false when there was none.
    pub(crate) fn unmount(&self, name: &str) -> bool {
        let index = {
            let mut slots = lock(&self.slots);
            let Some(at) = slots.0.iter().position(|(_, n)| n == name) else {
                return false;
            };
            slots.0.remove(at).0
        };
        self.mounts.unmount(&api_path(index))
    }

    pub(crate) fn routes(&self) -> MountRoutes {
        self.mounts.routes()
    }

    pub(crate) fn indices(&self) -> Vec<UNib32> {
        lock(&self.slots)
            .0
            .iter()
            .map(|(i, _)| UNib32(*i))
            .collect()
    }

    pub(crate) fn index_of(&self, name: &str) -> Option<u32> {
        let slots = lock(&self.slots);
        slots.0.iter().find(|(_, n)| n == name).map(|(i, _)| *i)
    }

    pub(crate) fn name_of(&self, index: u32) -> Option<String> {
        let slots = lock(&self.slots);
        slots
            .0
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, n)| n.clone())
    }

    pub(crate) fn presence(&self, index: u32) -> MountPresenceOwned {
        self.mounts.presence(&api_path(index))
    }
}

/// The path of `ext[index].api`.
fn api_path(index: u32) -> Vec<ResourceIndex> {
    [EXT_ID, index, API_ID]
        .into_iter()
        .map(|id| ResourceIndex::new(UNib32(id)))
        .collect()
}

/// The `ext` index a path of the mount table names; `None` for another path.
pub(crate) fn index_of_path(path: &[ResourceIndex]) -> Option<u32> {
    match path {
        [ext, index, api] if ext.get().0 == EXT_ID && api.get().0 == API_ID => Some(index.get().0),
        _ => None,
    }
}

/// The fake's own API, whose traits come before the mounted ones' for global paths.
fn own_api() -> ww_self::ApiBundleOwned {
    let bytes = ww_self::compressed::bundle_bytes(&api_impl::WW_SELF_BYTES)
        .expect("the generated introspection data decompresses");
    ww_self::ApiBundleOwned::from_ww_bytes_owned(&bytes)
        .expect("the generated introspection data decodes")
}
