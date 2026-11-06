#!/usr/bin/env bash
# Fails if Cargo.lock contains a server, CLI, gRPC or OpenSSL crate.
# minotari-wallet-core is used with `default-features = false, features = ["http"]`;
# any of these means such a dependency crept back in (utoipa-swagger-ui also
# downloads during its build). Usage: check_forbidden_crates.sh [Cargo.lock]
set -euo pipefail
lock="${1:-Cargo.lock}"
found=0
for crate in openssl-sys native-tls utoipa-swagger-ui axum clap tonic prost; do
  if grep -qx "name = \"$crate\"" "$lock"; then
    echo "::error::$lock contains the forbidden crate $crate"
    found=1
  fi
done
exit $found
