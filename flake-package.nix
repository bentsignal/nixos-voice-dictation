{ lib, rustPlatform, pkg-config, cmake, llvmPackages, alsa-lib, libxkbcommon }:

rustPlatform.buildRustPackage {
  pname = "whisrs";
  version = "0.1.20-local";
  src = ./.;
  cargoLock.lockFile = ./Cargo.lock;
  nativeBuildInputs = [ pkg-config cmake llvmPackages.clang rustPlatform.bindgenHook ];
  buildInputs = [ alsa-lib libxkbcommon ];
  postInstall = ''
    install -Dm644 contrib/whisrs.service $out/lib/systemd/user/whisrs.service
  '';
  meta = {
    description = "Local voice dictation, patched for a standalone Right Alt hotkey";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
