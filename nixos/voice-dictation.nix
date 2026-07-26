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
      ExecStart = "${project}/contrib/asr-sidecars/parakeet-sherpa/.venv/bin/python ${project}/contrib/asr-sidecars/parakeet-sherpa/server.py --model-dir ${model} --threads 8 --screen-context";
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
    after = [ "graphical-session.target" "parakeet-sidecar.service" ];
    wants = [ "parakeet-sidecar.service" ];
    serviceConfig = {
      Type = "simple";
      ExecStart = "${whisrs}/bin/whisrsd";
      Restart = "on-failure";
      RestartSec = 3;
    };
    environment.ALSA_CONFIG_PATH = "${project}/nixos/whisrs-alsa.conf";
  };
}
