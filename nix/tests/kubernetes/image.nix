{ pkgs, autoscaler, gradient, gradient-nix }:
let
  driver = (autoscaler.override {
    cargoBuildExtraArgs = "--example prepared-kubernetes";
  }).overrideAttrs (old: {
    pname = "prepared-kubernetes";
    meta = old.meta // { mainProgram = "prepared-kubernetes"; };
  });
in
pkgs.dockerTools.buildLayeredImage {
  name = "gradient-kubernetes-test";
  tag = "fixture";
  includeNixDB = true;
  contents = [
    driver
    gradient
    gradient-nix
    pkgs.bash
    pkgs.cacert
    pkgs.coreutils
    pkgs.curl
    pkgs.libargon2
    pkgs.openssh
    pkgs.postgresql_18
  ];
  fakeRootCommands = ''
    mkdir -p etc/nix tmp root var/empty
    chmod 1777 tmp
    printf '%s\n' \
      'root:x:0:0:root:/root:/bin/bash' \
      'postgres:x:1000:1000:PostgreSQL:/pgdata:/bin/bash' \
      'nobody:x:65532:65532:Test runner:/tmp:/bin/bash' > etc/passwd
    printf '%s\n' 'root:x:0:' 'postgres:x:1000:' 'nobody:x:65532:' > etc/group
  '';
  config = {
    Env = [
      "PATH=/bin"
      "HOME=/tmp"
      "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
      "NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
    ];
    Cmd = [ "/bin/bash" ];
  };
}
