#!/usr/bin/env bash
#
# Rebuilds the web console bundle the Rust binary embeds.
#
# The output — web/dist/index.html, a single self-contained file — is
# COMMITTED, because `webconsole::PAGE` include_str!s it at compile time and
# the Rust gates must not depend on a Node toolchain. Run this after touching
# anything under web/src and commit the result with the change.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../web"

npm install
npm run build
echo "built web/dist/index.html — commit it alongside your web/src changes"
