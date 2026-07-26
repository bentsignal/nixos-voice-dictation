{ config, lib, pkgs, ... }:

let
  whisrs = pkgs.callPackage /home/shawn/dev/whisrs/flake-package.nix { };
  project = "/home/shawn/dev/whisrs";
  model = "/home/shawn/.local/share/whisrs/models/parakeet-v3-int8";
  recoverKeyboardHotplug = pkgs.writeShellScript "recover-keyboard-hotplug" ''
    ${pkgs.systemd}/bin/systemctl --user stop whisrs.service
    sleep 1
    ${pkgs.systemd}/bin/systemctl --user restart xremap-copy-paste.service
    sleep 1
    ${pkgs.systemd}/bin/systemctl --user start whisrs.service
  '';
in
{
  users.users.shawn.extraGroups = [ "input" ];

  services.udev.extraRules = ''
    KERNEL=="uinput", SUBSYSTEM=="misc", MODE="0660", GROUP="input", TAG+="uaccess"
    KERNEL=="event*", SUBSYSTEM=="input", MODE="0660", GROUP="input", TAG+="uaccess"
    # This is a single-user workstation. Make newly connected physical
    # keyboards available immediately, even when the graphical login predates
    # the user's input-group membership.
    KERNEL=="event*", SUBSYSTEM=="input", ENV{ID_INPUT_KEYBOARD}=="1", OWNER="shawn", MODE="0600"
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

  # xremap 0.15 can notice that a keyboard disappeared without attaching to
  # the replacement evdev nodes. Restart xremap and then whisrs whenever the
  # stable input-device link directory changes.
  systemd.user.paths.keyboard-hotplug-recover = {
    description = "Watch for keyboard hotplug events";
    wantedBy = [ "default.target" ];
    after = [ "graphical-session.target" ];
    pathConfig = {
      PathChanged = "/dev/input/by-id";
      Unit = "keyboard-hotplug-recover.service";
    };
  };

  systemd.user.services.keyboard-hotplug-recover = {
    description = "Reconnect xremap and whisrs after keyboard hotplug";
    after = [ "graphical-session.target" ];
    serviceConfig = {
      Type = "oneshot";
      ExecStart = recoverKeyboardHotplug;
    };
  };
}
