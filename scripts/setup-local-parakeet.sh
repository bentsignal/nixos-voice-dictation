#!/usr/bin/env bash
# Build and install the repository's fully local Parakeet setup for one user.
# This path intentionally needs no Nix installation and does not write as root.

set -euo pipefail

MODEL_ARCHIVE="sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2"
MODEL_SHA256="5793d0fd397c5778d2cf2126994d58e9d56b1be7c04d13c7a15bb1b4eafb16bf"
MODEL_URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/$MODEL_ARCHIVE"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/whisrs"
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/whisrs"
systemd_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
sidecar_dir="$data_dir/parakeet-sidecar"
model_dir="$data_dir/models/parakeet-v3-int8"
bin_dir="$HOME/.local/bin"

for command in cargo curl sha256sum tar uv; do
    if ! command -v "$command" >/dev/null 2>&1; then
        printf 'Missing required command: %s\n' "$command" >&2
        if [ "$command" = uv ]; then
            printf 'Install uv with your package manager (Arch: sudo pacman -S uv).\n' >&2
        fi
        exit 1
    fi
done

printf '\n[1/6] Building whisrs for the local Parakeet backend...\n'
(
    cd "$repo_root"
    # Parakeet runs in the sidecar, so embedding whisper.cpp only adds CMake and
    # a second ASR runtime that this setup does not use.
    cargo build --release --locked --no-default-features --features tray,overlay
)
install -Dm755 "$repo_root/target/release/whisrs" "$bin_dir/whisrs"
install -Dm755 "$repo_root/target/release/whisrsd" "$bin_dir/whisrsd"

printf '\n[2/6] Creating the isolated Parakeet Python environment...\n'
install -d "$sidecar_dir"
install -m644 \
    "$repo_root/contrib/asr-sidecars/parakeet-sherpa/server.py" \
    "$repo_root/contrib/asr-sidecars/parakeet-sherpa/requirements.txt" \
    "$sidecar_dir/"
if [ ! -x "$sidecar_dir/.venv/bin/python" ]; then
    uv venv --python 3.12 "$sidecar_dir/.venv"
else
    printf 'Reusing %s\n' "$sidecar_dir/.venv"
fi
uv pip install --python "$sidecar_dir/.venv/bin/python" \
    -r "$sidecar_dir/requirements.txt"

printf '\n[3/6] Installing the Parakeet INT8 model...\n'
if [ -f "$model_dir/encoder.int8.onnx" ] \
    && [ -f "$model_dir/decoder.int8.onnx" ] \
    && [ -f "$model_dir/joiner.int8.onnx" ] \
    && [ -f "$model_dir/tokens.txt" ]; then
    printf 'Model is already present at %s\n' "$model_dir"
else
    download_dir="$(mktemp -d)"
    trap 'rm -rf "$download_dir"' EXIT
    curl --fail --location --progress-bar \
        --output "$download_dir/$MODEL_ARCHIVE" "$MODEL_URL"
    printf '%s  %s\n' "$MODEL_SHA256" "$download_dir/$MODEL_ARCHIVE" \
        | sha256sum --check --status
    tar -xjf "$download_dir/$MODEL_ARCHIVE" -C "$download_dir"
    extracted="$download_dir/${MODEL_ARCHIVE%.tar.bz2}"
    if [ ! -d "$extracted" ]; then
        printf 'Model archive did not contain the expected directory: %s\n' "$extracted" >&2
        exit 1
    fi
    install -d "$(dirname "$model_dir")"
    if [ -e "$model_dir" ]; then
        incomplete_model="$model_dir.incomplete.$(date +%s)"
        mv "$model_dir" "$incomplete_model"
        printf 'Moved incomplete model aside to %s\n' "$incomplete_model"
    fi
    mv "$extracted" "$model_dir"
    rm -rf "$download_dir"
    trap - EXIT
fi

printf '\n[4/6] Installing user services...\n'
install -Dm644 "$repo_root/contrib/parakeet-sidecar.service" \
    "$systemd_dir/parakeet-sidecar.service"
install -Dm644 "$repo_root/contrib/whisrs-local.service" \
    "$systemd_dir/whisrs.service"

printf '\n[5/6] Configuring whisrs...\n'
install -d -m700 "$config_dir"
if [ ! -e "$config_dir/config.toml" ]; then
    audio_device=default
    if command -v arecord >/dev/null 2>&1 && arecord -L 2>/dev/null | grep -qx pulse; then
        audio_device=pulse
    fi

    cat >"$config_dir/config.toml" <<EOF
[general]
backend = "asr-sidecar"
language = "en"
notify = true
remove_filler_words = true
audio_feedback = true
audio_feedback_volume = 0.5
tray = true
overlay = true

[audio]
device = "$audio_device"

[input]
key_delay_ms = 2
backend = "auto"

[asr-sidecar]
url = "http://127.0.0.1:8765/transcribe"
model = "parakeet-tdt-0.6b-v3-int8"

[hotkeys]
toggle = "RightAlt"

[overlay]
theme = "carbon"
width = 100
height = 40
EOF
    chmod 600 "$config_dir/config.toml"
else
    printf 'Keeping existing config: %s\n' "$config_dir/config.toml"
fi

printf '\n[6/6] Starting services...\n'
systemctl --user daemon-reload
systemctl --user enable parakeet-sidecar.service whisrs.service
systemctl --user restart parakeet-sidecar.service whisrs.service

printf '\nLocal dictation is installed.\n'
printf 'Binaries: %s/{whisrs,whisrsd}\n' "$bin_dir"
printf 'Config:   %s/config.toml\n' "$config_dir"
printf 'Model:    %s\n' "$model_dir"

if ! [ -r /dev/uinput ] || ! find /dev/input -maxdepth 1 -name 'event*' -readable \
    -print -quit 2>/dev/null | grep -q .; then
    printf '\nRight Alt still needs input-device permission. Run once:\n' >&2
    printf '  sudo usermod -aG input %q\n' "$USER" >&2
    printf 'Then log out and back in before testing the hotkey.\n' >&2
fi
