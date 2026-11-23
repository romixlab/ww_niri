//! ww_niri's own volume indicator: one overlay item, shown on every change of the default sink's
//! volume or mute and for [LASTS] after the last, at the place the `volume-indicator` line of
//! `ww.kdl` gives.
//!
//! The compositor owns it as [OWNER], an owner no connection ever is, so no client can lift or
//! replace it and it never takes another item's key.

use std::time::Duration;

use ww_niri_api::{
    Anchor, At, BackdropOwned, ContentOwned, ItemOwned, PlaceOwned, Px, SurfaceOwned,
};

/// The compositor as an item's owner; connections are numbered from 1.
pub const OWNER: u64 = 0;
/// The indicator's key.
pub const KEY: &str = "volume";
/// How long it stays after the last change.
pub const LASTS: Duration = Duration::from_millis(1500);
/// Its priority, in the band of status of what plays.
const PRIORITY: u8 = 224;

/// Where the indicator sits and what it says, from `ww.kdl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Indicator {
    pub anchor: Anchor,
    pub dx: i32,
    pub dy: i32,
    /// An output by connector name; `None`: the focused one.
    pub output: Option<String>,
    pub label: String,
    /// The label while the sink is muted.
    pub muted_label: String,
}

impl Default for Indicator {
    /// Top right, 40 px in from both edges.
    fn default() -> Self {
        Self {
            anchor: Anchor::TopRight,
            dx: -40,
            dy: 40,
            output: None,
            label: "Volume".to_owned(),
            muted_label: "Muted".to_owned(),
        }
    }
}

impl Indicator {
    /// The item for a sink at `volume` percent, `muted` or not.
    pub fn item(&self, volume: u16, muted: bool) -> ItemOwned {
        let label = if muted {
            &self.muted_label
        } else {
            &self.label
        };
        ItemOwned {
            place: PlaceOwned {
                on: SurfaceOwned::Output(self.output.clone()),
                at: At::Anchor {
                    anchor: self.anchor,
                    dx: Px::new(self.dx),
                    dy: Px::new(self.dy),
                },
            },
            content: ContentOwned::Meter {
                label: label.clone(),
                percent: volume,
            },
            backdrop: BackdropOwned {
                blur: 2,
                dim: 70,
                ..BackdropOwned::default()
            },
            priority: PRIORITY,
            lasts: Some(exact_units::Fixed::new(
                u32::try_from(LASTS.as_millis()).expect("1.5 s fits"),
            )),
            mute: false,
            dismissible: false,
            report_shown: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_item_passes_the_api_checks() {
        let indicator = Indicator::default();
        for (volume, muted) in [(0, false), (45, false), (150, true)] {
            let item = indicator.item(volume, muted);
            assert_eq!(item.check(KEY), Ok(Some(LASTS)));
        }
        let ContentOwned::Meter { label, percent } = indicator.item(30, true).content else {
            panic!("a meter");
        };
        assert_eq!((label.as_str(), percent), ("Muted", 30));
    }
}
