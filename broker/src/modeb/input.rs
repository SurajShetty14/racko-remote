//! Phase 4: browser input back-channel (WebRTC data channel JSON → [`ironrdp_input`] operations).
//!
//! Messages (one JSON object per data channel message):
//!
//! ```text
//! {"type":"mousemove","x":640,"y":512}
//! {"type":"mousedown","button":"left","x":640,"y":512}      // also "mouseup"; button: left|middle|right
//! {"type":"wheel","deltaX":0,"deltaY":100,"deltaMode":0}    // WheelEvent fields
//! {"type":"keydown","code":"KeyA","repeat":false}           // also "keyup"; KeyboardEvent.code
//! ```
//!
//! Coordinates are desktop pixels. `KeyboardEvent.code` is mapped to RDP scancodes
//! with the table the web client uses (`iron-remote-desktop/src/lib/scancodes.ts`),
//! embedded at build time so both clients share one source of truth.

use std::collections::HashMap;

use anyhow::{Context as _, bail};
use ironrdp_input::{MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use serde::Deserialize;

const WEB_SCANCODES_TS: &str = include_str!("../../../web-client/iron-remote-desktop/src/lib/scancodes.ts");

/// Same scaling as the web client (`ironrdp-web` `wheel_rotations`).
const LINES_TO_PIXELS: f64 = 50.0;
const PAGES_TO_PIXELS: f64 = 38.0 * LINES_TO_PIXELS;

/// Modifier and lock keys whose auto-repeat is dropped, like the web client does.
const NO_REPEAT_CODES: &[&str] = &[
    "ShiftLeft",
    "ShiftRight",
    "ControlLeft",
    "ControlRight",
    "AltLeft",
    "AltRight",
    "MetaLeft",
    "MetaRight",
    "CapsLock",
    "NumLock",
    "ScrollLock",
];

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum InputMsg {
    Mousemove {
        x: f64,
        y: f64,
    },
    Mousedown {
        button: Button,
        x: f64,
        y: f64,
    },
    Mouseup {
        button: Button,
        x: f64,
        y: f64,
    },
    Wheel {
        #[serde(rename = "deltaX", default)]
        delta_x: f64,
        #[serde(rename = "deltaY", default)]
        delta_y: f64,
        #[serde(rename = "deltaMode", default)]
        delta_mode: u8,
    },
    Keydown {
        code: String,
        #[serde(default)]
        repeat: bool,
    },
    Keyup {
        code: String,
    },
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Button {
    Left,
    Middle,
    Right,
}

impl From<Button> for MouseButton {
    fn from(button: Button) -> Self {
        match button {
            Button::Left => Self::Left,
            Button::Middle => Self::Middle,
            Button::Right => Self::Right,
        }
    }
}

/// Translates browser input JSON into `ironrdp-input` operations for one desktop size.
pub(super) struct InputTranslator {
    scancodes: HashMap<&'static str, Scancode>,
    max_x: f64,
    max_y: f64,
}

impl InputTranslator {
    pub(super) fn new(desktop_width: u16, desktop_height: u16) -> anyhow::Result<Self> {
        let scancodes = web_client_scancodes();
        if scancodes.get("KeyA") != Some(&Scancode::from_u16(0x001E)) {
            bail!("web client scancode table did not parse (KeyA missing); check scancodes.ts format");
        }
        Ok(Self {
            scancodes,
            max_x: f64::from(desktop_width.saturating_sub(1)),
            max_y: f64::from(desktop_height.saturating_sub(1)),
        })
    }

    pub(super) fn key_count(&self) -> usize {
        self.scancodes.len()
    }

    /// Parse one data channel message. Unknown key codes and dropped repeats yield no operations.
    pub(super) fn translate(&self, json: &str) -> anyhow::Result<Vec<Operation>> {
        let msg: InputMsg = serde_json::from_str(json).context("decode input message")?;
        let ops = match msg {
            InputMsg::Mousemove { x, y } => vec![Operation::MouseMove(self.position(x, y))],
            InputMsg::Mousedown { button, x, y } => vec![
                Operation::MouseMove(self.position(x, y)),
                Operation::MouseButtonPressed(button.into()),
            ],
            InputMsg::Mouseup { button, x, y } => vec![
                Operation::MouseMove(self.position(x, y)),
                Operation::MouseButtonReleased(button.into()),
            ],
            InputMsg::Wheel {
                delta_x,
                delta_y,
                delta_mode,
            } => {
                let scale = match delta_mode {
                    1 => LINES_TO_PIXELS,
                    2 => PAGES_TO_PIXELS,
                    _ => 1.0,
                };
                // Browser deltas grow downward/rightward; RDP wheel units are the opposite sign.
                [(true, delta_y), (false, delta_x)]
                    .into_iter()
                    .filter(|&(_, delta)| delta != 0.0)
                    .map(|(is_vertical, delta)| {
                        Operation::WheelRotations(WheelRotations {
                            is_vertical,
                            rotation_units: (-delta * scale).round().clamp(f64::from(i16::MIN), f64::from(i16::MAX))
                                as i16,
                        })
                    })
                    .collect()
            }
            InputMsg::Keydown { code, repeat } => {
                if repeat && NO_REPEAT_CODES.contains(&code.as_str()) {
                    Vec::new()
                } else {
                    self.scancode(&code).map(Operation::KeyPressed).into_iter().collect()
                }
            }
            InputMsg::Keyup { code } => self.scancode(&code).map(Operation::KeyReleased).into_iter().collect(),
        };
        Ok(ops)
    }

    fn position(&self, x: f64, y: f64) -> MousePosition {
        MousePosition {
            x: x.round().clamp(0.0, self.max_x) as u16,
            y: y.round().clamp(0.0, self.max_y) as u16,
        }
    }

    fn scancode(&self, code: &str) -> Option<Scancode> {
        let scancode = self.scancodes.get(code).copied();
        if scancode.is_none() {
            tracing::debug!(code, "No RDP scancode for KeyboardEvent.code; ignoring");
        }
        scancode
    }
}

/// `KeyboardEvent.code` → scancode, parsed from the web client's `scancodes.ts`.
///
/// Mirrors its Blink table: `scanCodeToKeyCode` + `scanCodeToKeyCodeExtras`,
/// inverted so later entries win (as `invertCodesMapping` does). Gecko-only
/// names (e.g. `VolumeMute`) are added when they do not collide.
fn web_client_scancodes() -> HashMap<&'static str, Scancode> {
    let mut blink = HashMap::new();
    let mut gecko = Vec::new();
    let mut section = None;

    for line in WEB_SCANCODES_TS.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("const ").and_then(|l| l.strip_suffix(" = {")) {
            section = Some(name);
            continue;
        }
        if line.starts_with("};") {
            section = None;
            continue;
        }
        let Some((scancode, code)) = parse_entry(line) else {
            continue;
        };
        match section {
            Some("scanCodeToKeyCode" | "scanCodeToKeyCodeExtras") => {
                blink.insert(code, Scancode::from_u16(scancode));
            }
            Some("scanCodeToKeyCodeGeckoExtras") => gecko.push((code, Scancode::from_u16(scancode))),
            _ => {}
        }
    }

    for (code, scancode) in gecko {
        blink.entry(code).or_insert(scancode);
    }
    return blink;

    /// `'0xE01C': 'NumpadEnter',` → `(0xE01C, "NumpadEnter")`; computed values are skipped.
    fn parse_entry(line: &str) -> Option<(u16, &str)> {
        let rest = line.strip_prefix("'0x")?;
        let (hex, rest) = rest.split_once('\'')?;
        let rest = rest.strip_prefix(':')?.trim_start().strip_prefix('\'')?;
        let (code, _) = rest.split_once('\'')?;
        Some((u16::from_str_radix(hex, 16).ok()?, code))
    }
}
