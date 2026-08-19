#!/usr/bin/env bash
set -euo pipefail

# rust-cache recursively treats every directory named `tests` below a Cargo target directory as
# a nested compiler target. `cargo package` leaves unpacked crate sources under target/package,
# so the action probes nonexistent tests/target and tests/trybuild directories and emits failure
# annotations while saving an otherwise valid cache. The unpacked test sources have already been
# verified and are not compiler artifacts; removing them avoids the false errors without dropping
# the package verification targets that make subsequent runs fast.
if [[ -d target/package ]]; then
  find target/package -mindepth 2 -maxdepth 2 -type d -name tests -exec rm -rf {} +
fi
