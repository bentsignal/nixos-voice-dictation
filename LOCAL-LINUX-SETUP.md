# Local voice dictation on NixOS and other Linux distributions

This fork supports the same private, on-device dictation workflow on both of
Shawn's KDE Wayland machines. The application code is portable Rust; only the
host integration differs:

- NixOS uses [`nixos/voice-dictation.nix`](nixos/voice-dictation.nix).
- Arch, EndeavourOS, and other systemd distributions use
  [`scripts/setup-local-parakeet.sh`](scripts/setup-local-parakeet.sh).

Both paths provide Right Alt press-to-toggle dictation, the bottom-centre HUD,
Unicode text injection, and local Parakeet TDT 0.6B v3 INT8 transcription via
sherpa-onnx. Audio never leaves the machine.

## Arch / EndeavourOS

Install the host build tools and `uv` once:

```console
sudo pacman -S --needed base-devel rust alsa-lib libxkbcommon clang uv
```

Then run the rootless per-user installer from this checkout:

```console
./scripts/setup-local-parakeet.sh
```

The script builds the sidecar-only whisrs variant, creates an isolated Python
3.12 environment, downloads and verifies the official sherpa-onnx INT8 model,
and enables the two systemd user services. Existing whisrs configuration is
never overwritten. When available, it selects the Pulse input explicitly so
PipeWire's configured default source is honored instead of a hardware ALSA
fallback. The included user service selects the US XKB layout used on these two
machines; change `XKB_DEFAULT_LAYOUT` in `whisrs-local.service` when installing
on a workstation with a different layout.

The built-in Right Alt hotkey reads Linux input events. If the account is not
already in the `input` group, run `sudo usermod -aG input "$USER"` and log out
and back in once. `/dev/uinput` also needs the rule from
[`contrib/99-whisrs.rules`](contrib/99-whisrs.rules); desktop logind ACLs often
already provide access.

Useful checks:

```console
systemctl --user status parakeet-sidecar whisrs
curl http://127.0.0.1:8765/health
~/.local/bin/whisrs status
```

## NixOS

The existing NixOS integration and its machine-specific notes remain in
[`LOCAL-NIXOS-SETUP.md`](LOCAL-NIXOS-SETUP.md). It builds the same source tree
through `flake-package.nix` and manages equivalent user services declaratively.
