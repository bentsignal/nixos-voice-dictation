{ config, lib, pkgs, ... }:

let
  whisrs = pkgs.callPackage /home/shawn/dev/whisrs/flake-package.nix { };
  project = "/home/shawn/dev/whisrs";
  model = "/home/shawn/.local/share/whisrs/models/parakeet-v3-int8";
in
{
  users.users.shawn.extraGroups = [ "input" ];

  services.udev.extraRules = ''
    KERNEL=="uinput", SUBSYSTEM=="misc", MODE="0660", GROUP="input", TAG+="uaccess"
    KERNEL=="event*", SUBSYSTEM=="input", MODE="0660", GROUP="input", TAG+="uaccess"
    # xremap creates this virtual device after login, so grant the desktop
    # user access even when the current session predates input-group membership.
    KERNEL=="event*", SUBSYSTEM=="input", ATTRS{name}=="xremap", OWNER="shawn", MODE="0600"
  '';

  environment.systemPackages = [ whisrs pkgs.tesseract ];

  systemd.user.services.parakeet-sidecar = {
    description = "Local Parakeet speech recognition with screen OCR context";
    wantedBy = [ "default.target" ];
    after = [ "graphical-session.target" "pipewire.service" ];
    path = [ pkgs.kdePackages.spectacle pkgs.tesseract ];
    serviceConfig = {
      Type = "simple";
      WorkingDirectory = "${project}/contrib/asr-sidecars/parakeet-sherpa";
      ExecStart = "${project}/contrib/asr-sidecars/parakeet-sherpa/.venv/bin/python ${project}/contrib/asr-sidecars/parakeet-sherpa/server.py --model-dir ${model} --threads 8 --screen-context-toggle-file /home/shawn/.config/whisrs/ocr-corrections-enabled";
      Restart = "on-failure";
      RestartSec = 3;
    };
    environment = {
      LD_LIBRARY_PATH = lib.makeLibraryPath [ pkgs.stdenv.cc.cc.lib pkgs.zlib ];
    };
  };

  systemd.user.services.whisrs = {
    description = "Always-on local voice dictation";
    wantedBy = [ "default.target" ];
    # xremap exclusively grabs the physical keyboard and forwards unchanged
    # keys through its virtual device. Start after it so Right Alt is observed
    # on that stable, compositor-visible output rather than the grabbed device.
    after = [
      "graphical-session.target"
      "parakeet-sidecar.service"
      "xremap-copy-paste.service"
    ];
    wants = [ "parakeet-sidecar.service" "xremap-copy-paste.service" ];
    serviceConfig = {
      Type = "simple";
      ExecStart = "${whisrs}/bin/whisrsd";
      Restart = "on-failure";
      RestartSec = 3;
    };
    path = [ pkgs.kdePackages.kdialog pkgs.wl-clipboard ];
    environment.ALSA_CONFIG_PATH = "${project}/nixos/whisrs-alsa.conf";
  };
}
