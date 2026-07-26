//! Global hotkey listener via evdev input devices.
//!
//! Passively monitors keyboard input devices for configured key combos
//! and sends commands to the daemon when they match.

mod parse;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use evdev::{Device, EventType, InputEventKind, Key};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{Command, HotkeyConfig};
pub use parse::{parse_hotkey, HotkeyBinding};

/// Maximum number of attempts to find keyboard input devices.
const HOTKEY_MAX_RETRIES: u32 = 10;

/// Initial retry delay (doubles each attempt, capped at 10 s).
const HOTKEY_INITIAL_DELAY: Duration = Duration::from_secs(1);
const QUICK_MENU_HOLD: Duration = Duration::from_secs(2);

/// A configured hotkey action.
struct HotkeyAction {
    binding: HotkeyBinding,
    command: Command,
}

/// Start the global hotkey listener.
///
/// Enumerates keyboard input devices, listens for key events, and sends
/// matching commands through the provided channel. Retries with exponential
/// backoff if no keyboards are found yet (common on boot when the daemon
/// starts before input devices are fully initialized). Runs until dropped.
pub async fn start_hotkey_listener(config: &HotkeyConfig, cmd_tx: mpsc::Sender<Command>) {
    let mut actions = Vec::new();

    if let Some(ref s) = config.toggle {
        match parse_hotkey(s) {
            Ok(binding) => {
                info!("hotkey: toggle = {s}");
                actions.push(HotkeyAction {
                    binding,
                    command: Command::Toggle { language: None },
                });
            }
            Err(e) => warn!("invalid toggle hotkey '{s}': {e}"),
        }
    }

    if let Some(ref s) = config.cancel {
        match parse_hotkey(s) {
            Ok(binding) => {
                info!("hotkey: cancel = {s}");
                actions.push(HotkeyAction {
                    binding,
                    command: Command::Cancel,
                });
            }
            Err(e) => warn!("invalid cancel hotkey '{s}': {e}"),
        }
    }

    if let Some(ref s) = config.command {
        match parse_hotkey(s) {
            Ok(binding) => {
                info!("hotkey: command = {s}");
                actions.push(HotkeyAction {
                    binding,
                    command: Command::CommandMode,
                });
            }
            Err(e) => warn!("invalid command hotkey '{s}': {e}"),
        }
    }

    if let Some(ref s) = config.speak {
        match parse_hotkey(s) {
            Ok(binding) => {
                info!("hotkey: speak = {s}");
                actions.push(HotkeyAction {
                    binding,
                    command: Command::Speak,
                });
            }
            Err(e) => warn!("invalid speak hotkey '{s}': {e}"),
        }
    }

    if actions.is_empty() {
        debug!("no hotkeys configured");
        return;
    }

    // Find keyboard input devices, retrying with backoff on boot.
    let mut delay = HOTKEY_INITIAL_DELAY;
    let mut devices = Vec::new();

    for attempt in 1..=HOTKEY_MAX_RETRIES {
        match enumerate_keyboards() {
            Ok(d) if !d.is_empty() => {
                if attempt > 1 {
                    info!("found {} keyboard device(s) (attempt {attempt})", d.len());
                }
                devices = d;
                break;
            }
            Ok(_) => {
                if attempt == HOTKEY_MAX_RETRIES {
                    warn!(
                        "no keyboard input devices found after {HOTKEY_MAX_RETRIES} attempts — hotkeys disabled"
                    );
                    return;
                }
                info!(
                    "no keyboard devices found (attempt {attempt}/{HOTKEY_MAX_RETRIES}) — retrying in {delay:?}"
                );
            }
            Err(e) => {
                if attempt == HOTKEY_MAX_RETRIES {
                    warn!(
                        "failed to enumerate input devices after {HOTKEY_MAX_RETRIES} attempts: {e} — hotkeys disabled"
                    );
                    return;
                }
                info!(
                    "failed to enumerate input devices (attempt {attempt}/{HOTKEY_MAX_RETRIES}): {e} — retrying in {delay:?}"
                );
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(10));
    }

    info!(
        "hotkey listener monitoring {} keyboard device(s)",
        devices.len()
    );

    // Spawn a listener task for each device.
    for device in devices {
        let name = device.name().unwrap_or("unknown").to_string();
        let actions_clone: Vec<(Vec<Key>, Key, Command)> = actions
            .iter()
            .map(|a| {
                (
                    a.binding.modifiers.clone(),
                    a.binding.trigger,
                    a.command.clone(),
                )
            })
            .collect();
        let tx = cmd_tx.clone();

        tokio::spawn(async move {
            if let Err(e) = listen_device(device, &actions_clone, tx).await {
                debug!("hotkey listener for '{name}' stopped: {e}");
            }
        });
    }
}

/// Enumerate all keyboard input devices.
fn enumerate_keyboards() -> anyhow::Result<Vec<Device>> {
    let mut keyboards = Vec::new();
    let input_dir = Path::new("/dev/input");

    if !input_dir.exists() {
        anyhow::bail!("/dev/input does not exist");
    }

    for entry in std::fs::read_dir(input_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Only look at eventN devices.
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.starts_with("event") {
            continue;
        }

        match Device::open(&path) {
            Ok(device) => {
                // Composite USB keyboards sometimes expose modifier keys on a
                // secondary event node that does not advertise the full A-Z
                // key set. Include any node that can emit Right Alt so a
                // standalone RightAlt toggle is not silently missed.
                if let Some(keys) = device.supported_keys() {
                    let dev_name = device.name().unwrap_or("unknown").to_string();
                    let is_virtual_keyboard = dev_name == "whisrs virtual keyboard";
                    let looks_like_keyboard =
                        keys.contains(Key::KEY_A) && keys.contains(Key::KEY_LEFTMETA);
                    if !is_virtual_keyboard
                        && (looks_like_keyboard || keys.contains(Key::KEY_RIGHTALT))
                    {
                        debug!("found keyboard: {} ({})", dev_name, path.display());
                        keyboards.push(device);
                    }
                }
            }
            Err(e) => {
                debug!("cannot open {}: {e}", path.display());
            }
        }
    }

    Ok(keyboards)
}

/// Listen on a single device for hotkey combos.
async fn listen_device(
    device: Device,
    actions: &[(Vec<Key>, Key, Command)],
    cmd_tx: mpsc::Sender<Command>,
) -> anyhow::Result<()> {
    // Track which keys are currently held.
    let mut held_keys: HashSet<Key> = HashSet::new();
    let mut pending_toggles: HashMap<Key, (Instant, Arc<AtomicBool>)> = HashMap::new();

    // Wrap device in async fd.
    let mut stream = device.into_event_stream()?;

    loop {
        let event = stream.next_event().await?;

        if event.event_type() != EventType::KEY {
            continue;
        }

        let key = match event.kind() {
            InputEventKind::Key(k) => k,
            _ => continue,
        };

        match event.value() {
            1 => {
                // Key press.
                held_keys.insert(key);

                // Check if any hotkey combo matches.
                for (modifiers, trigger, command) in actions {
                    if key == *trigger && modifiers_held(&held_keys, modifiers) {
                        // A standalone toggle fires on release so we can
                        // distinguish a quick tap from a two-second hold.
                        if matches!(command, Command::Toggle { .. }) && modifiers.is_empty() {
                            let cancelled = Arc::new(AtomicBool::new(false));
                            pending_toggles.insert(key, (Instant::now(), Arc::clone(&cancelled)));
                            let hold_tx = cmd_tx.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(QUICK_MENU_HOLD).await;
                                if !cancelled.swap(true, Ordering::SeqCst) {
                                    debug!("toggle key held — opening quick menu");
                                    let _ = hold_tx.send(Command::QuickMenu).await;
                                }
                            });
                            continue;
                        }
                        debug!("hotkey matched: {:?}", command);
                        let _ = cmd_tx.send(command.clone()).await;
                    }
                }
            }
            0 => {
                // Key release.
                held_keys.remove(&key);
                if let Some((pressed_at, cancelled)) = pending_toggles.remove(&key) {
                    // If the hold task has not claimed the gesture, it was a
                    // tap. Preserve the existing one-tap start/stop behavior.
                    if !cancelled.swap(true, Ordering::SeqCst)
                        && pressed_at.elapsed() < QUICK_MENU_HOLD
                    {
                        debug!("toggle key tapped");
                        let _ = cmd_tx.send(Command::Toggle { language: None }).await;
                    }
                }
            }
            _ => {} // Repeat (2) — ignore.
        }
    }
}

/// Check if all required modifier keys (or their left/right variants) are held.
fn modifiers_held(held: &HashSet<Key>, required: &[Key]) -> bool {
    required.iter().all(|m| {
        // Accept either left or right variant.
        match *m {
            Key::KEY_LEFTMETA => {
                held.contains(&Key::KEY_LEFTMETA) || held.contains(&Key::KEY_RIGHTMETA)
            }
            Key::KEY_LEFTALT => {
                held.contains(&Key::KEY_LEFTALT) || held.contains(&Key::KEY_RIGHTALT)
            }
            Key::KEY_LEFTCTRL => {
                held.contains(&Key::KEY_LEFTCTRL) || held.contains(&Key::KEY_RIGHTCTRL)
            }
            Key::KEY_LEFTSHIFT => {
                held.contains(&Key::KEY_LEFTSHIFT) || held.contains(&Key::KEY_RIGHTSHIFT)
            }
            other => held.contains(&other),
        }
    })
}
