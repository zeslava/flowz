#!/bin/sh
set -e
cargo clippy --workspace -- -D warnings
