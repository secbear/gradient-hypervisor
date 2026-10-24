{ lib
, bashInteractive
, buildEnv
, cacert
, closureInfo
, coreutils
, dockerTools
, git
, gradient
, gradient-nix
, openssh
, runCommand
, writeTextDir
}:
let
  nixConf = writeTextDir "etc/nix/nix.conf" ''
    experimental-features = nix-command flakes
    build-users-group =
    sandbox-fallback = true
    max-jobs = auto
  '';

  users = runCommand "gradient-worker-users" { } ''
    mkdir -p $out/etc
    echo 'root:x:0:0:root:/root:/bin/sh' > $out/etc/passwd
    echo 'nobody:x:65534:65534:nobody:/var/empty:/bin/false' >> $out/etc/passwd
    echo 'root:x:0:' > $out/etc/group
    echo 'nogroup:x:65534:' >> $out/etc/group
  '';

  root = buildEnv {
    name = "gradient-worker-root";
    paths = [ bashInteractive cacert coreutils git gradient gradient-nix openssh nixConf users ];
    pathsToLink = [ "/bin" "/etc" ];
  };
in
dockerTools.buildLayeredImage {
  name = "gradient-worker";
  tag = gradient.version;
  contents = [ root ];

  # The worker's local store must see the image's store paths as valid.
  extraCommands = ''
    mkdir -p nix/var/nix/gcroots root tmp var/lib/gradient-worker
    chmod 1777 tmp
    NIX_REMOTE="local?root=$PWD" USER=nobody \
      ${gradient-nix}/bin/nix-store --load-db < ${closureInfo { rootPaths = [ root ]; }}/registration
    ln -s ${root} nix/var/nix/gcroots/gradient-worker
  '';

  config = {
    Entrypoint = [ (lib.getExe' gradient "gradient-worker") ];
    WorkingDir = "/var/lib/gradient-worker";
    Env = [
      "PATH=/bin"
      "HOME=/root"
      "USER=root"
      "SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt"
      "NIX_SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt"
      "GRADIENT_WORKER_NIX_BIN=/bin/nix"
      "GRADIENT_WORKER_SSH_BIN=/bin/ssh"
    ];
  };
}
