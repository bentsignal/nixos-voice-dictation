# NixOS voice dictation setup

This fork contains the machine-local integration used on Shawn's KDE Plasma
Wayland workstation:

- tap **Right Alt** once to begin recording and once again to stop;
- a bottom-centre, click-through HUD indicates recording/transcription;
- local NVIDIA Parakeet TDT 0.6B v3 INT8 inference through sherpa-onnx;
- local Tesseract screen OCR used as a conservative spelling hint;
- Unicode text injection through `/dev/uinput`;
- automatic, hidden startup through systemd user services;
- Logitech C920 microphone capture.

No cancel hotkey is configured. In particular, Escape does not cancel an
active recording.

## Machine integration

The active NixOS module is [`nixos/voice-dictation.nix`](nixos/voice-dictation.nix).
It installs the patched `whisrs`, grants input/uinput access, installs
Tesseract, and defines the two user services.

The module currently contains paths and the username for this workstation.
Adjust `project`, `model`, and `users.users.shawn` before using it elsewhere,
then import it from `/etc/nixos/configuration.nix`:

```nix
imports = [
  ./hardware-configuration.nix
  ./voice-dictation.nix
];
```

Apply with:

```console
sudo nixos-rebuild switch
```

Log out and back in once after the first rebuild so membership in the `input`
group reaches the graphical session.

## Runtime files not stored in Git

- `contrib/asr-sidecars/parakeet-sherpa/.venv`
- `~/.local/share/whisrs/models/parakeet-v3-int8`
- `~/.config/whisrs/config.toml`
- Rust/Nix build outputs

The checked-in `requirements.txt` records the sidecar's Python dependencies.
The runtime config should use `backend = "asr-sidecar"`, enable the overlay,
and set `[hotkeys] toggle = "RightAlt"`.
