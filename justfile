# Local development installs. Releases go through CI on a v* tag; these keep the working tree out of Homebrew's copy.

# Build the release binary and install it at ~/.local/bin/overseer, which precedes the Homebrew prefix in PATH.
install:
    cargo build --release --locked
    mkdir -p "$HOME/.local/bin"
    install -m 755 target/release/overseer "$HOME/.local/bin/overseer"

# Every check CI runs.
check:
    cargo fmt --all --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo nextest run --locked
    cargo shear
    cargo deny check
    typos
