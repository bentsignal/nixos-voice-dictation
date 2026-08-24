//! Audio capture using the `cpal` crate.
//!
//! Opens the configured input device at 16kHz mono 16-bit and pushes audio
//! chunks into a tokio mpsc channel for downstream processing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, StreamConfig};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::AudioChunk;

/// Desired sample rate for speech recognition.
pub const SAMPLE_RATE: u32 = 16_000;

/// Number of channels (mono).
const CHANNELS: u16 = 1;

/// Prefix used for a specific native PipeWire source. The suffix is the
/// stable PipeWire `node.name` passed to `pw-record --target`.
pub const PIPEWIRE_NODE_PREFIX: &str = "pipewire-node:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDeviceChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PipeWireCaptureLayout {
    channels: u16,
    channel_map: Option<String>,
    apple_t2: bool,
}

impl Default for PipeWireCaptureLayout {
    fn default() -> Self {
        Self {
            channels: CHANNELS,
            channel_map: None,
            apple_t2: false,
        }
    }
}

/// Return logical microphone choices for the tray.
///
/// On PipeWire, enumerate source nodes directly. This yields one entry per
/// physical/logical microphone (including Apple T2 internal microphones)
/// instead of ALSA's repetitive `front`/`digital`/`surround` PCM profiles.
/// CPAL/ALSA remains the fallback on systems without PipeWire tools.
pub fn input_device_choices(configured_device: &str) -> Vec<InputDeviceChoice> {
    let mut choices = pipewire_source_choices();
    if !choices.is_empty() {
        choices.insert(
            0,
            InputDeviceChoice {
                id: "pipewire".to_string(),
                label: "PipeWire default".to_string(),
            },
        );
    } else {
        choices.push(InputDeviceChoice {
            id: "default".to_string(),
            label: "System default".to_string(),
        });
        choices.extend(
            cpal_input_device_names()
                .into_iter()
                .map(|name| InputDeviceChoice {
                    label: name.clone(),
                    id: name,
                }),
        );
    }

    if !choices.iter().any(|choice| choice.id == configured_device) {
        choices.insert(
            0,
            InputDeviceChoice {
                id: configured_device.to_string(),
                label: configured_device.to_string(),
            },
        );
    }

    let mut seen = std::collections::HashSet::new();
    choices.retain(|choice| seen.insert(choice.id.clone()));
    choices
}

fn cpal_input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    let mut names: Vec<String> = host
        .input_devices()
        .map(|devices| devices.filter_map(|device| device.name().ok()).collect())
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}

fn pipewire_source_choices() -> Vec<InputDeviceChoice> {
    let Some(objects) = pipewire_objects() else {
        return Vec::new();
    };
    pipewire_source_choices_from(&objects)
}

fn pipewire_objects() -> Option<serde_json::Value> {
    let output = std::process::Command::new("pw-dump").output().ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

fn pipewire_source_choices_from(objects: &serde_json::Value) -> Vec<InputDeviceChoice> {
    let Some(objects) = objects.as_array() else {
        return Vec::new();
    };

    let mut choices: Vec<InputDeviceChoice> = objects
        .iter()
        .filter(|object| {
            object.get("type").and_then(|v| v.as_str()) == Some("PipeWire:Interface:Node")
        })
        .filter_map(|object| object.get("info")?.get("props"))
        .filter(|props| props.get("media.class").and_then(|v| v.as_str()) == Some("Audio/Source"))
        .filter_map(|props| {
            let node_name = props.get("node.name")?.as_str()?;
            let label = props
                .get("node.description")
                .and_then(|v| v.as_str())
                .or_else(|| props.get("node.nick").and_then(|v| v.as_str()))
                .unwrap_or(node_name);
            Some(InputDeviceChoice {
                id: format!("{PIPEWIRE_NODE_PREFIX}{node_name}"),
                label: label.to_string(),
            })
        })
        .collect();
    choices.sort_by_key(|choice| choice.label.to_lowercase());
    choices
}

fn pipewire_capture_layout(target: &str) -> PipeWireCaptureLayout {
    pipewire_objects()
        .as_ref()
        .and_then(|objects| pipewire_capture_layout_from(objects, target))
        .unwrap_or_default()
}

fn pipewire_capture_layout_from(
    objects: &serde_json::Value,
    target: &str,
) -> Option<PipeWireCaptureLayout> {
    let props = objects.as_array()?.iter().find_map(|object| {
        let props = object.get("info")?.get("props")?;
        (props.get("media.class")?.as_str()? == "Audio/Source"
            && props.get("node.name")?.as_str()? == target)
            .then_some(props)
    })?;

    let channels = props
        .get("audio.channels")?
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())?;
    if channels <= CHANNELS || channels > 64 {
        return Some(PipeWireCaptureLayout::default());
    }

    let apple_t2 = ["alsa.card_name", "api.alsa.card.name"]
        .into_iter()
        .filter_map(|key| props.get(key).and_then(|value| value.as_str()))
        .any(|name| name == "Apple T2 Audio");

    let positions = props.get("audio.position")?.as_str()?;
    let channel_map = parse_pipewire_channel_map(positions, channels)?;
    Some(PipeWireCaptureLayout {
        channels,
        channel_map: Some(channel_map),
        apple_t2,
    })
}

fn parse_pipewire_channel_map(positions: &str, channels: u16) -> Option<String> {
    let positions = positions.trim().strip_prefix('[')?.strip_suffix(']')?;
    let names: Vec<&str> = positions
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    if names.len() != usize::from(channels)
        || names
            .iter()
            .any(|name| !name.chars().all(|c| c.is_ascii_alphanumeric()))
    {
        return None;
    }
    Some(names.join(","))
}

/// A handle to a running audio capture session.
///
/// The actual `cpal::Stream` lives on a dedicated thread (since it's not Send).
/// This handle provides only the receiver and a stop signal.
pub struct AudioCaptureHandle {
    /// Receiver end of the audio channel.
    receiver: Option<mpsc::UnboundedReceiver<AudioChunk>>,
    /// Signal to stop the capture thread.
    stop_signal: Arc<AtomicBool>,
    /// Join handle for the capture thread.
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

// SAFETY: The AudioCaptureHandle itself only contains Send types.
// The non-Send cpal::Stream lives on its own thread.
unsafe impl Send for AudioCaptureHandle {}

impl Drop for AudioCaptureHandle {
    fn drop(&mut self) {
        // Signal the capture thread to stop.
        self.stop_signal.store(true, Ordering::Release);
        // Wait for the thread to finish (non-async, best-effort).
        if let Some(handle) = self.thread_handle.take() {
            handle.join().ok();
        }
    }
}

impl AudioCaptureHandle {
    /// Start capturing audio from the default input device.
    ///
    /// The capture runs on a dedicated thread. Audio chunks are sent through
    /// the internal channel; call `take_receiver()` to get the receiving end.
    pub fn start() -> anyhow::Result<Self> {
        Self::start_with_level_tx(None)
    }

    /// Start capturing audio and optionally publish a normalized volume level.
    pub fn start_with_level_tx(
        level_tx: Option<tokio::sync::watch::Sender<f32>>,
    ) -> anyhow::Result<Self> {
        Self::start_with_device_and_level_tx("default", level_tx)
    }

    /// Start capturing from a named input device and optionally publish a
    /// normalized volume level. The special name `default` selects the host's
    /// default input device.
    pub fn start_with_device_and_level_tx(
        device_name: &str,
        level_tx: Option<tokio::sync::watch::Sender<f32>>,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel::<AudioChunk>();
        let stop_signal = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop_signal);
        let device_name = device_name.to_string();

        // Channel to send back any initialization error from the thread.
        let (init_tx, init_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();

        let thread_handle = std::thread::Builder::new()
            .name("whisrs-audio".into())
            .spawn(move || {
                run_capture(tx, stop_clone, init_tx, level_tx, device_name);
            })
            .context("failed to spawn audio capture thread")?;

        // Wait for initialization result.
        let init_result = init_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("audio capture thread exited unexpectedly"))?;
        init_result?;

        Ok(Self {
            receiver: Some(rx),
            stop_signal,
            thread_handle: Some(thread_handle),
        })
    }

    /// Take the receiver end of the audio channel.
    pub fn take_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<AudioChunk>> {
        self.receiver.take()
    }

    /// Signal the capture thread to stop (async-friendly).
    /// The channel will close once the thread exits. Callers reading
    /// from the receiver will see `None` after remaining chunks drain.
    pub fn stop(&mut self) {
        self.stop_signal.store(true, Ordering::Release);
    }

    /// Stop the audio capture and return all accumulated samples from the channel.
    pub async fn stop_and_collect(mut self) -> anyhow::Result<Vec<i16>> {
        // Signal the capture thread to stop.
        self.stop_signal.store(true, Ordering::Release);

        // Wait for the thread to finish.
        if let Some(handle) = self.thread_handle.take() {
            // Use spawn_blocking to avoid blocking the tokio runtime.
            tokio::task::spawn_blocking(move || {
                handle.join().ok();
            })
            .await?;
        }

        let mut all_samples = Vec::new();

        if let Some(mut rx) = self.receiver.take() {
            // Drain all remaining chunks from the channel.
            rx.close();
            while let Ok(chunk) = rx.try_recv() {
                all_samples.extend_from_slice(&chunk);
            }
        }

        info!("captured {} audio samples", all_samples.len());
        Ok(all_samples)
    }
}

/// Run the audio capture on the current thread.
///
/// Sends the initialization result through `init_tx`, then blocks until the
/// stop signal is set. The cpal Stream lives on this thread (it's not Send).
fn run_capture(
    tx: mpsc::UnboundedSender<AudioChunk>,
    stop_signal: Arc<AtomicBool>,
    init_tx: std::sync::mpsc::Sender<anyhow::Result<()>>,
    level_tx: Option<tokio::sync::watch::Sender<f32>>,
    configured_device: String,
) {
    let result = setup_and_run(tx, stop_signal, &init_tx, level_tx, &configured_device);
    if let Err(e) = result {
        // If init_tx hasn't been used yet, send the error.
        init_tx.send(Err(e)).ok();
    }
}

fn setup_and_run(
    tx: mpsc::UnboundedSender<AudioChunk>,
    stop_signal: Arc<AtomicBool>,
    init_tx: &std::sync::mpsc::Sender<anyhow::Result<()>>,
    level_tx: Option<tokio::sync::watch::Sender<f32>>,
    configured_device: &str,
) -> anyhow::Result<()> {
    if let Some(target) = configured_device.strip_prefix(PIPEWIRE_NODE_PREFIX) {
        return setup_and_run_pipewire(tx, stop_signal, init_tx, level_tx, target);
    }

    let host = cpal::default_host();
    let device = if configured_device.eq_ignore_ascii_case("default") {
        host.default_input_device()
            .ok_or_else(|| anyhow::anyhow!("no default audio input device found"))?
    } else {
        host.input_devices()
            .context("failed to enumerate audio input devices")?
            .find(|device| device.name().is_ok_and(|name| name == configured_device))
            .ok_or_else(|| anyhow::anyhow!("audio input device not found: {configured_device}"))?
    };

    let device_name = device.name().unwrap_or_else(|_| "unknown".into());
    info!("using audio input device: {device_name}");

    let config = StreamConfig {
        channels: CHANNELS,
        sample_rate: SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    // Verify device support.
    let supported = device
        .supported_input_configs()
        .context("failed to query supported input configs")?;

    let mut found_match = false;
    for range in supported {
        if range.channels() == CHANNELS
            && range.min_sample_rate().0 <= SAMPLE_RATE
            && range.max_sample_rate().0 >= SAMPLE_RATE
            && range.sample_format() == SampleFormat::I16
        {
            found_match = true;
            break;
        }
    }

    if !found_match {
        warn!(
            "device may not natively support {SAMPLE_RATE}Hz mono i16; \
             cpal will attempt conversion"
        );
    }

    let err_callback = |err: cpal::StreamError| {
        error!("audio stream error: {err}");
    };

    let callback_level_tx = level_tx.clone();
    let stream = device
        .build_input_stream(
            &config,
            move |data: &[i16], _info: &cpal::InputCallbackInfo| {
                if let Some(level_tx) = &callback_level_tx {
                    let _ = level_tx.send(audio_level(data));
                }
                if tx.send(data.to_vec()).is_err() {
                    // Channel closed — capture is stopping.
                }
            },
            err_callback,
            None,
        )
        .context("failed to build audio input stream")?;

    stream.play().context("failed to start audio stream")?;
    debug!("audio capture started at {SAMPLE_RATE}Hz mono i16");

    // Signal successful initialization.
    init_tx.send(Ok(())).ok();

    // Block until stop is signaled. Keep the stream alive.
    while !stop_signal.load(Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    debug!("audio capture stopping");
    if let Some(level_tx) = &level_tx {
        let _ = level_tx.send(0.0);
    }
    drop(stream);

    Ok(())
}

fn setup_and_run_pipewire(
    tx: mpsc::UnboundedSender<AudioChunk>,
    stop_signal: Arc<AtomicBool>,
    init_tx: &std::sync::mpsc::Sender<anyhow::Result<()>>,
    level_tx: Option<tokio::sync::watch::Sender<f32>>,
    target: &str,
) -> anyhow::Result<()> {
    use std::io::Read;
    use std::process::Stdio;

    let layout = pipewire_capture_layout(target);
    let sample_rate = SAMPLE_RATE.to_string();
    let channels = layout.channels.to_string();
    let mut command = std::process::Command::new("pw-record");
    command.args([
        "--target",
        target,
        "--rate",
        &sample_rate,
        "--channels",
        &channels,
    ]);
    if let Some(channel_map) = &layout.channel_map {
        command.args(["--channel-map", channel_map]);
    }
    let mut child = command
        .args(["--format", "s16", "--raw", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start pw-record for the selected PipeWire microphone")?;

    // Give PipeWire a moment to resolve and link the requested node so an
    // invalid/stale target fails before the daemon reports recording started.
    std::thread::sleep(std::time::Duration::from_millis(150));
    if let Some(status) = child.try_wait().context("failed to poll pw-record")? {
        let mut stderr = String::new();
        if let Some(mut stream) = child.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        anyhow::bail!(
            "pw-record could not open PipeWire microphone {target}: {}{}",
            status,
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(" — {}", stderr.trim())
            }
        );
    }

    let mut stdout = child
        .stdout
        .take()
        .context("pw-record stdout was not available")?;
    init_tx.send(Ok(())).ok();
    info!(
        "using PipeWire input source: {target} ({} channel{}{})",
        layout.channels,
        if layout.channels == 1 { "" } else { "s" },
        if layout.apple_t2 {
            ", Apple T2 microphone conditioning enabled"
        } else {
            ""
        }
    );

    let mut bytes = [0_u8; 3200];
    let mut pending_byte = None;
    let mut pending_samples = Vec::new();
    let channels = usize::from(layout.channels);
    let mut mono_processor = PipeWireMonoProcessor::new(channels, layout.apple_t2);
    while !stop_signal.load(Ordering::Acquire) {
        let count = stdout
            .read(&mut bytes)
            .context("failed to read pw-record audio")?;
        if count == 0 {
            break;
        }
        let mut sample_bytes = Vec::with_capacity(count + 1);
        if let Some(byte) = pending_byte.take() {
            sample_bytes.push(byte);
        }
        sample_bytes.extend_from_slice(&bytes[..count]);
        if sample_bytes.len() % 2 != 0 {
            pending_byte = sample_bytes.pop();
        }
        pending_samples.extend(
            sample_bytes
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
        );
        let complete_len = pending_samples.len() / channels * channels;
        if complete_len == 0 {
            continue;
        }
        let samples = mono_processor.process(&pending_samples[..complete_len]);
        pending_samples.drain(..complete_len);
        if let Some(level_tx) = &level_tx {
            let _ = level_tx.send(audio_level(&samples));
        }
        if tx.send(samples).is_err() {
            break;
        }
    }

    let _ = child.kill();
    let status = child.wait().context("failed to wait for pw-record")?;
    if let Some(level_tx) = &level_tx {
        let _ = level_tx.send(0.0);
    }
    debug!("PipeWire audio capture stopped ({status})");
    Ok(())
}

/// Stateful downmix and conditioning for native PipeWire sources.
///
/// Apple T2 microphone arrays expose quiet, largely unprocessed raw channels.
/// The normal desktop DSP applies a substantial gain stage and DC/rumble
/// filtering before applications see the microphone. Keep that workaround
/// local to whisrs so USB microphones and the machine's global speaker/audio
/// configuration are unaffected.
struct PipeWireMonoProcessor {
    channels: usize,
    apple_t2: bool,
    selected_channel: Option<usize>,
    previous_input: f32,
    previous_output: f32,
}

impl PipeWireMonoProcessor {
    fn new(channels: usize, apple_t2: bool) -> Self {
        Self {
            channels: channels.max(1),
            apple_t2,
            selected_channel: None,
            previous_input: 0.0,
            previous_output: 0.0,
        }
    }

    fn process(&mut self, samples: &[i16]) -> Vec<i16> {
        if !self.apple_t2 {
            return strongest_channel_mono(samples, self.channels);
        }

        let channel = *self
            .selected_channel
            .get_or_insert_with(|| strongest_channel_index(samples, self.channels));
        // One-pole 120 Hz high-pass at the 16 kHz capture rate, followed by
        // the same broad gain magnitude used by the community T2 mic DSP.
        const HIGH_PASS_ALPHA: f32 = 0.955;
        const GAIN: f32 = 80.0;
        samples
            .chunks_exact(self.channels)
            .map(|frame| {
                let input = frame[channel] as f32 / i16::MAX as f32;
                let filtered =
                    HIGH_PASS_ALPHA * (self.previous_output + input - self.previous_input);
                self.previous_input = input;
                self.previous_output = filtered;
                (filtered * GAIN)
                    .clamp(-1.0, 1.0)
                    .mul_add(i16::MAX as f32, 0.0) as i16
            })
            .collect()
    }
}

/// Reduce an interleaved microphone stream to mono by retaining the channel
/// with the strongest signal in this chunk. This handles microphone arrays
/// whose PipeWire positions cannot be remixed to `MONO` (notably Apple T2's
/// AUX0/AUX1/AUX2 array), and is also useful for multichannel audio interfaces
/// where only one input is connected.
fn strongest_channel_mono(samples: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return samples.to_vec();
    }

    let strongest = strongest_channel_index(samples, channels);
    samples
        .chunks_exact(channels)
        .map(|frame| frame[strongest])
        .collect()
}

fn strongest_channel_index(samples: &[i16], channels: usize) -> usize {
    let mut energy = vec![0_u64; channels.max(1)];
    for frame in samples.chunks_exact(channels) {
        for (channel, sample) in frame.iter().enumerate() {
            let sample = i64::from(*sample);
            energy[channel] = energy[channel].saturating_add((sample * sample) as u64);
        }
    }
    energy
        .iter()
        .enumerate()
        .max_by_key(|(_, value)| *value)
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn audio_level(data: &[i16]) -> f32 {
    if data.is_empty() {
        return 0.0;
    }

    let mut peak = 0.0_f32;
    let sum_squares: f32 = data
        .iter()
        .map(|sample| {
            let normalized = (*sample as f32 / i16::MAX as f32).abs();
            peak = peak.max(normalized);
            normalized * normalized
        })
        .sum();
    let rms = (sum_squares / data.len() as f32).sqrt();

    // RMS alone changes slowly across normal speech and can make the HUD look
    // pinned at one height. Blend in the instantaneous peak so consonants and
    // syllable attacks remain visible, while RMS keeps the meter from becoming
    // a jittery peak indicator. The gentler compressor preserves more dynamic
    // range than the former RMS-only k=18 curve.
    let envelope = rms * 0.35 + peak * 0.65;
    (1.0 - (-envelope * 7.0).exp()).clamp(0.0, 1.0)
}

/// Encode raw PCM samples (16kHz, mono, i16) to a WAV byte buffer.
pub fn encode_wav(samples: &[i16]) -> anyhow::Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer =
            hound::WavWriter::new(&mut cursor, spec).context("failed to create WAV writer")?;

        for &sample in samples {
            writer
                .write_sample(sample)
                .context("failed to write WAV sample")?;
        }

        writer.finalize().context("failed to finalize WAV")?;
    }

    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_wav_produces_valid_output() {
        let samples: Vec<i16> = (0..1600).map(|i| (i % 256) as i16).collect();
        let wav = encode_wav(&samples).unwrap();

        // WAV files start with "RIFF".
        assert_eq!(&wav[..4], b"RIFF");

        // Verify we can read it back with hound.
        let cursor = std::io::Cursor::new(&wav);
        let reader = hound::WavReader::new(cursor).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);

        let read_samples: Vec<i16> = reader.into_samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(read_samples.len(), 1600);
        assert_eq!(read_samples, samples);
    }

    #[test]
    fn encode_wav_empty_samples() {
        let wav = encode_wav(&[]).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
    }

    #[test]
    fn audio_level_reacts_to_transient_peaks() {
        let steady = vec![1_000_i16; 256];
        let mut transient = steady.clone();
        transient[128] = 12_000;

        assert!(audio_level(&transient) > audio_level(&steady) * 2.0);
    }

    #[test]
    fn audio_level_silence_is_zero() {
        assert_eq!(audio_level(&[0; 256]), 0.0);
    }

    #[test]
    fn pipewire_choices_are_one_per_source_node() {
        let dump = serde_json::json!([
            {
                "type": "PipeWire:Interface:Node",
                "info": { "props": {
                    "media.class": "Audio/Source",
                    "node.name": "alsa_input.internal",
                    "node.description": "Internal Microphone"
                }}
            },
            {
                "type": "PipeWire:Interface:Node",
                "info": { "props": {
                    "media.class": "Audio/Sink",
                    "node.name": "alsa_output.speakers",
                    "node.description": "Speakers"
                }}
            }
        ]);

        assert_eq!(
            pipewire_source_choices_from(&dump),
            vec![InputDeviceChoice {
                id: format!("{PIPEWIRE_NODE_PREFIX}alsa_input.internal"),
                label: "Internal Microphone".to_string(),
            }]
        );
    }

    #[test]
    fn pipewire_capture_preserves_multichannel_array_layout() {
        let dump = serde_json::json!([
            {
                "type": "PipeWire:Interface:Node",
                "info": { "props": {
                    "media.class": "Audio/Source",
                    "node.name": "alsa_input.apple_t2",
                    "alsa.card_name": "Apple T2 Audio",
                    "audio.channels": 3,
                    "audio.position": "[ AUX0, AUX1, AUX2 ]"
                }}
            }
        ]);

        assert_eq!(
            pipewire_capture_layout_from(&dump, "alsa_input.apple_t2"),
            Some(PipeWireCaptureLayout {
                channels: 3,
                channel_map: Some("AUX0,AUX1,AUX2".to_string()),
                apple_t2: true,
            })
        );
    }

    #[test]
    fn pipewire_capture_layout_rejects_mismatched_positions() {
        let dump = serde_json::json!([
            {
                "info": { "props": {
                    "media.class": "Audio/Source",
                    "node.name": "broken",
                    "audio.channels": 3,
                    "audio.position": "[ AUX0, AUX1 ]"
                }}
            }
        ]);

        assert_eq!(pipewire_capture_layout_from(&dump, "broken"), None);
    }

    #[test]
    fn strongest_channel_is_selected_for_mono_capture() {
        // Three interleaved channels; the middle channel carries the signal.
        let samples = [1, 100, 2, 3, -200, 4, 5, 300, 6];
        assert_eq!(strongest_channel_mono(&samples, 3), vec![100, -200, 300]);
    }

    #[test]
    fn mono_capture_is_unchanged() {
        let samples = [1, -2, 3, -4];
        assert_eq!(strongest_channel_mono(&samples, 1), samples);
    }

    #[test]
    fn apple_t2_conditioning_boosts_weak_audio() {
        let mut processor = PipeWireMonoProcessor::new(3, true);
        let mut input = Vec::new();
        for index in 0..800 {
            let sample = if index % 20 < 10 { 100 } else { -100 };
            input.extend_from_slice(&[sample, sample / 2, sample / 4]);
        }
        let output = processor.process(&input);
        assert_eq!(output.len(), 800);
        assert!(output.iter().map(|sample| sample.abs()).max().unwrap() > 2_000);
    }

    #[test]
    fn apple_t2_conditioning_removes_steady_dc() {
        let mut processor = PipeWireMonoProcessor::new(3, true);
        let output = processor.process(&[500_i16; 3 * 2_000]);
        assert!(output.last().unwrap().abs() < 10);
    }

    /// Opt-in hardware check used by maintainers to exercise a real PipeWire
    /// source without making the normal test suite depend on audio hardware.
    #[tokio::test]
    async fn selected_pipewire_source_captures_audio_when_requested() {
        let Ok(target) = std::env::var("WHISRS_TEST_PIPEWIRE_TARGET") else {
            return;
        };
        let capture = AudioCaptureHandle::start_with_device_and_level_tx(&target, None).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        let samples = capture.stop_and_collect().await.unwrap();
        assert!(
            !samples.is_empty(),
            "selected PipeWire source returned no audio"
        );
        assert!(
            samples.iter().any(|sample| *sample != 0),
            "selected PipeWire source returned only silence"
        );
    }
}
