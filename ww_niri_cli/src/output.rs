//! Printing: one JSON value and one line of text per piece of data, picked by `--json`.

use std::time::Duration;

use serde_json::{Value, json};
use ww_niri_api::{
    BrowserPhase, BrowserStateOwned, InfoOwned, OutputOwned, PlaybackStateOwned, PlaybackStatus,
    PlayerInfoOwned, PlayerSourceOwned, PointerPosition, SinkOwned, WindowOwned,
};
use ww_niri_client::{AudioState, Snapshot};

/// Prints `value` as pretty JSON, or `text` for people.
pub fn emit(json: bool, value: Value, text: &str) {
    if json {
        anstream::println!(
            "{}",
            serde_json::to_string_pretty(&value).expect("a json::Value always serializes")
        );
    } else {
        anstream::println!("{text}");
    }
}

/// Green for a playing/running status, dim for a stopped or paused one, plain otherwise.
pub fn status_color(label: &str) -> anstyle::Style {
    use anstyle::{AnsiColor, Effects, Style};
    match label {
        "playing" | "running" => Style::new().fg_color(Some(AnsiColor::Green.into())),
        "paused" | "starting" | "stopping" => Style::new().fg_color(Some(AnsiColor::Yellow.into())),
        "stopped" => Style::new().effects(Effects::DIMMED),
        _ => Style::new(),
    }
}

/// "5 s", "200 ms": for a connect timeout in an error message.
pub fn human_duration(d: Duration) -> String {
    if d.as_millis() % 1000 == 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{} ms", d.as_millis())
    }
}

pub fn info(i: &InfoOwned) -> (Value, String) {
    let value = json!({
        "niri_version": i.niri_version,
        "ww_niri_version": i.ww_niri_version,
        "hostname": i.hostname,
    });
    let text = format!(
        "{}  niri {}  ww_niri {}",
        i.hostname, i.niri_version, i.ww_niri_version
    );
    (value, text)
}

fn window_value(w: &WindowOwned) -> Value {
    json!({
        "id": w.id.0,
        "app_id": w.app_id,
        "title": w.title,
        "pid": w.pid,
        "workspace": w.workspace.map(|w| w.0),
        "output": w.output,
        "focused": w.focused,
        "fullscreen": w.fullscreen,
        "browser_slot": w.browser.map(|s| s.0),
    })
}

fn window_line(w: &WindowOwned) -> String {
    let mut flags = Vec::new();
    if w.focused {
        flags.push("focused");
    }
    if w.fullscreen {
        flags.push("fullscreen");
    }
    let flags = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(", "))
    };
    format!(
        "{}  {}{flags}",
        w.id.0,
        w.title
            .as_deref()
            .unwrap_or(w.app_id.as_deref().unwrap_or("(untitled)"))
    )
}

pub fn windows(ws: &[WindowOwned]) -> (Value, String) {
    let value = Value::Array(ws.iter().map(window_value).collect());
    let text = if ws.is_empty() {
        "no windows".to_owned()
    } else {
        ws.iter().map(window_line).collect::<Vec<_>>().join("\n")
    };
    (value, text)
}

pub fn outputs(os: &[OutputOwned]) -> (Value, String) {
    let value = Value::Array(
        os.iter()
            .map(|o| {
                json!({
                    "name": o.name,
                    "make": o.make,
                    "model": o.model,
                    "width": o.width.get(),
                    "height": o.height.get(),
                    "refresh_hz": f64::from(o.refresh.get()) / 1000.0,
                    "scale": f64::from(o.scale.get()) / 100.0,
                    "focused": o.focused,
                })
            })
            .collect(),
    );
    let text = os
        .iter()
        .map(|o| {
            format!(
                "{}  {}x{} @ {:.2} Hz, scale {:.2}  {} {}{}",
                o.name,
                o.width.get(),
                o.height.get(),
                f64::from(o.refresh.get()) / 1000.0,
                f64::from(o.scale.get()) / 100.0,
                o.make,
                o.model,
                if o.focused { "  [focused]" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    (
        value,
        if os.is_empty() {
            "no outputs".to_owned()
        } else {
            text
        },
    )
}

fn player_source_value(s: &PlayerSourceOwned) -> Value {
    match s {
        PlayerSourceOwned::Browser(slot) => json!({"kind": "browser", "slot": slot.0}),
        PlayerSourceOwned::Window { id, app_id } => {
            json!({"kind": "window", "id": id.0, "app_id": app_id})
        }
        PlayerSourceOwned::App { pid } => json!({"kind": "app", "pid": pid}),
        PlayerSourceOwned::Unknown(_) => json!({"kind": "unknown"}),
    }
}

fn player_source_line(s: &PlayerSourceOwned) -> String {
    match s {
        PlayerSourceOwned::Browser(slot) => format!("slot {}", slot.0),
        PlayerSourceOwned::Window { app_id, .. } => {
            format!("window {}", app_id.as_deref().unwrap_or("?"))
        }
        PlayerSourceOwned::App { pid } => {
            format!("pid {}", pid.map_or("?".to_owned(), |p| p.to_string()))
        }
        PlayerSourceOwned::Unknown(_) => "(unknown)".to_owned(),
    }
}

pub fn players(ps: &[PlayerInfoOwned]) -> (Value, String) {
    let value = Value::Array(
        ps.iter()
            .map(|p| {
                json!({
                    "id": p.id.0,
                    "identity": p.identity,
                    "source": player_source_value(&p.source),
                })
            })
            .collect(),
    );
    let text = ps
        .iter()
        .map(|p| {
            format!(
                "{}  {}  {}",
                p.id.0,
                p.identity,
                player_source_line(&p.source)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    (
        value,
        if ps.is_empty() {
            "no players".to_owned()
        } else {
            text
        },
    )
}

fn playback_status_label(s: PlaybackStatus) -> &'static str {
    match s {
        PlaybackStatus::Stopped => "stopped",
        PlaybackStatus::Playing => "playing",
        PlaybackStatus::Paused => "paused",
        PlaybackStatus::Unknown(_) => "unknown",
    }
}

pub fn playback_state(p: &PlaybackStateOwned) -> (Value, String) {
    let position_s = p.position.get() as f64 / 1_000_000.0;
    let length_s = p.length.map(|l| l.get() as f64 / 1_000_000.0);
    let value = json!({
        "status": playback_status_label(p.status),
        "title": p.title,
        "artist": p.artist,
        "position_s": position_s,
        "length_s": length_s,
        "rate": f64::from(p.rate.get()) / 1000.0,
        "can_seek": p.can_seek,
        "can_pause": p.can_pause,
    });
    let what = match (&p.title, &p.artist) {
        (Some(t), Some(a)) => format!("{a} - {t}"),
        (Some(t), None) => t.clone(),
        _ => "(untitled)".to_owned(),
    };
    let text = format!(
        "{}  {what}  {:.0}s{}",
        playback_status_label(p.status),
        position_s,
        length_s.map_or(String::new(), |l| format!("/{l:.0}s"))
    );
    (value, text)
}

fn browser_phase_label(phase: BrowserPhase) -> &'static str {
    match phase {
        BrowserPhase::Stopped => "stopped",
        BrowserPhase::Starting => "starting",
        BrowserPhase::Running => "running",
        BrowserPhase::Stopping => "stopping",
        BrowserPhase::Unknown(_) => "unknown",
    }
}

pub fn browser_state(s: &BrowserStateOwned) -> (Value, String) {
    let phase = browser_phase_label(s.phase);
    let value = json!({
        "phase": phase,
        "url": s.url,
        "title": s.title,
        "window": s.window.map(|w| w.0),
        "player": s.player.map(|p| p.0),
    });
    let text = format!(
        "{phase}{}",
        s.url.as_deref().map_or(String::new(), |u| format!("  {u}"))
    );
    (value, text)
}

pub fn audio(a: &AudioState) -> (Value, String) {
    let value = json!({
        "volume": a.volume,
        "muted": a.muted,
        "sink": a.sink.as_ref().map(sink_value),
    });
    let text = format!(
        "volume {}  {}  {}",
        a.volume.map_or("?".to_owned(), |v| format!("{v}%")),
        a.muted
            .map_or("?".to_owned(), |m| if m { "muted" } else { "unmuted" }
                .to_owned()),
        a.sink
            .as_ref()
            .map_or("(no sink)".to_owned(), |s| s.description.clone()),
    );
    (value, text)
}

fn sink_value(s: &SinkOwned) -> Value {
    json!({"name": s.name, "description": s.description})
}

/// One line of `wwn watch` per [Snapshot]: compact enough for a scrolling terminal or ndjson.
pub fn snapshot_line(json: bool, s: &Snapshot) -> String {
    let slots: Vec<Value> = s
        .slots
        .iter()
        .map(|slot| {
            let phase = slot
                .state
                .as_ref()
                .map_or("?", |b| browser_phase_label(b.phase));
            let status = slot
                .playback
                .as_ref()
                .map(|p| playback_status_label(p.state.status));
            json!({"index": slot.index, "phase": phase, "playback": status})
        })
        .collect();
    let (audio_value, audio_text) = audio(&s.audio);
    if json {
        let value = json!({
            "generation": s.generation,
            "windows": s.windows.len(),
            "focused": s.focused().map(|w| w.title.clone().or_else(|| w.app_id.clone())),
            "slots": slots,
            "players": s.players.len(),
            "audio": audio_value,
            "pointer": s.pointer.map(|p| pointer_position(&p).0),
        });
        serde_json::to_string(&value).expect("a json::Value always serializes")
    } else {
        let slot_text = s
            .slots
            .iter()
            .zip(&slots)
            .map(|(slot, v)| {
                let phase = v["phase"].as_str().unwrap_or("?");
                let color = status_color(phase);
                format!(
                    "slot{}:{color}{phase}{color:#}{}",
                    slot.index,
                    v["playback"]
                        .as_str()
                        .map_or(String::new(), |p| format!("/{p}"))
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "#{}  {} window(s)  {slot_text}  {audio_text}",
            s.generation,
            s.windows.len()
        )
    }
}

pub fn pointer_position(p: &PointerPosition) -> (Value, String) {
    let value = json!({
        "x": p.x.get(),
        "y": p.y.get(),
        "window": p.window.map(|w| json!({"id": w.id.0, "x": w.x.get(), "y": w.y.get()})),
    });
    let text = format!("({}, {})", p.x.get(), p.y.get());
    (value, text)
}
