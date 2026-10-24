{ lib, cacert, dockerTools, gradient-autoscaler }:
dockerTools.buildLayeredImage {
  name = "gradient-autoscaler";
  tag = gradient-autoscaler.version;
  contents = [ cacert ];

  config = {
    Entrypoint = [ (lib.getExe gradient-autoscaler) ];
    User = "65532:65532";
    ExposedPorts."8080/tcp" = { };
    Env = [
      "SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt"
      "GRADIENT_AUTOSCALER_CONFIG=/etc/gradient-autoscaler/config.json"
    ];
  };
}
