{ pkgs ? import <nixpkgs> { }, distfiles ? /tmp/pkg-ports-dist }:

let
  lib = pkgs.lib;
  source = lib.cleanSourceWith {
    src = ../.;
    filter = path: type:
      let base = baseNameOf path;
      in !(builtins.elem base [ ".git" "target" "live" ".build" ".build-output.log" "result" ]);
  };
  stagedPorts = ./.build/ports;

  tools = pkgs.rustPlatform.buildRustPackage {
    pname = "yakimo-tools";
    version = "0.1.0";
    src = source;
    cargoLock.lockFile = ../Cargo.lock;
    cargoBuildFlags = [ "--workspace" "--bins" "--examples" ];
    doCheck = false;
    auditable = false;

    installPhase = ''
      runHook preInstall
      mkdir -p "$out/bin"
      pkg_bin=$(find target -type f -path '*/release/pkg-cli' -print -quit)
      init_bin=$(find target -type f -path '*/release/pkg-init' -print -quit)
      repo_bin=$(find target -type f -path '*/release/examples/ports2repo' -print -quit)
      [ -n "$pkg_bin" ] && [ -n "$init_bin" ] && [ -n "$repo_bin" ] || {
        echo "expected workspace binaries were not built" >&2
        exit 1
      }
      install -m 755 "$pkg_bin" "$out/bin/pkg"
      install -m 755 "$init_bin" "$out/bin/pkg-init"
      install -m 755 "$repo_bin" "$out/bin/ports2repo"
      runHook postInstall
    '';

    meta.mainProgram = "pkg";
  };

  repo = pkgs.runCommand "yakimo-live-test-repo" {
    nativeBuildInputs = [ tools ];
  } ''
    ports2repo "$out" http://127.0.0.1:8784 \
      --distfiles ${distfiles} ${stagedPorts}
  '';

  liveCommand = pkgs.writeShellScriptBin "yakimo-live"
    (builtins.readFile ./yakimo-live.sh);

  nixos = import (pkgs.path + "/nixos") {
    system = "x86_64-linux";
    configuration = { config, lib, pkgs, ... }: {
      imports = [
        <nixpkgs/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix>
      ];

      networking.hostName = "yakimo-live";
      networking.networkmanager.enable = lib.mkForce false;
      services.openssh.enable = lib.mkForce false;
      services.getty.autologinUser = lib.mkForce "nixos";
      services.getty.helpLine = lib.mkForce ''
        Yaki Imo disposable package-manager test host (TEST KEYS ONLY).
        Run 'yakimo-live smoke' to build and verify real packages.
        Run 'yakimo-live shell' for an isolated PKG_ROOT shell.
      '';
      programs.bash.promptInit = "PS1='YAKIMO# '";

      boot.kernelParams = [ "console=tty0" "console=ttyS0,115200n8" ];
      boot.zfs.forceImportRoot = false;
      systemd.services."serial-getty@ttyS0".enable = true;
      boot.loader.grub.memtest86.enable = lib.mkForce false;
      documentation.nixos.enable = lib.mkForce false;

      isoImage.volumeID = lib.mkForce "YAKIMO_TEST";
      isoImage.edition = lib.mkForce "yakimo-test";

      environment.etc."yakimo/repo".source = repo;
      environment.etc."yakimo/ports".source = stagedPorts;
      environment.etc."yakimo/PORT-SELECTION".text = builtins.readFile ./ports.txt;
      environment.systemPackages = [ tools liveCommand ] ++ (with pkgs; [
        bubblewrap
        gcc
        binutils
        gnumake
        perl
        python3
        coreutils
        gnutar
        gzip
        sqlite
        gnused
        gnugrep
        gawk
        diffutils
        curl
        xz
        findutils
        util-linux
        bison
        m4
        gettext
        flex
        gperf
        bzip2
        dash
        glibc.bin
        pkgconf
      ]);
    };
  };
in
{
  inherit tools repo;
  iso = nixos.config.system.build.isoImage;
}
