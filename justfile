# agent_tools — common workflows
#
# `just --list` shows what this project supports. First fmt/clippy/test gate
# for this repo (previously ungated — nothing had ever run fmt or clippy here).
#
# FreeBSD note: /tmp is noexec, so .cargo/config.toml pins TMPDIR=target/tmp
# for cargo subprocesses — but nothing creates that dir in a fresh clone or
# jj workspace, and build scripts (e.g. ring's) then die with
# "cc: unable to make temporary file". Build-ish recipes depend on _tmpdir.
#
# RUSTC_WRAPPER= dodges the (sometimes broken) host sccache.
#
# bead: at-fmt-clippy-justfile-baseline-gls

set shell := ["bash", "-cu"]

# Default: list every available recipe.
list:
    @just --list

# ── pre-PR gate ────────────────────────────────────────────────────────────

# Full gate: fmt-check + clippy (deny warnings) + test. Run before any PR.
check: fmt-check clippy test

# ── individual steps ───────────────────────────────────────────────────────

# Create the TMPDIR that .cargo/config.toml points cargo subprocesses at.
_tmpdir:
    @mkdir -p target/tmp

# Reformat the workspace in place.
fmt:
    RUSTC_WRAPPER= cargo fmt --all

# Check formatting without editing files.
fmt-check:
    RUSTC_WRAPPER= cargo fmt --all -- --check

# Lint the whole workspace; warnings are errors so the gate stays at zero.
clippy: _tmpdir
    RUSTC_WRAPPER= cargo clippy --workspace --all-targets -- -D warnings

# Run the test suite.
test: _tmpdir
    RUSTC_WRAPPER= cargo test --workspace

# Release build (what the ~/.local/bin symlinks point at).
build: _tmpdir
    RUSTC_WRAPPER= cargo build --release
