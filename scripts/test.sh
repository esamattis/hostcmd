#!/bin/sh

set -eux

cargo build
cargo test

python3 -m unittest discover -s tests

cargo clippy
cargo fmt -- --check
