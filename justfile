# Local development installs. Releases go through CI on a v* tag; these keep the working tree out of Homebrew's copy.

# Recipe parameters reach the shell as positional arguments, so `"$@"` keeps each one whole.
set positional-arguments

# Build the release binary and install it at ~/.local/bin/overseer.
install:
    @cargo build --release --locked
    @mkdir -p "$HOME/.local/bin"
    @install -m 755 target/release/overseer "$HOME/.local/bin/overseer"
    # Prove the install took: a shell that already hashed another `overseer` keeps running that one.
    @resolved=$(command -v overseer || echo nothing); \
    if [ "$resolved" = "$HOME/.local/bin/overseer" ]; then \
      echo "installed $resolved"; \
    else \
      echo "installed $HOME/.local/bin/overseer, but a fresh shell resolves overseer to $resolved"; \
      echo "run 'hash -r' (bash) or 'rehash' (zsh) in any shell still running the old binary"; \
    fi

# Run the built binary by path: a shell may still hash Homebrew's copy under the same name.
run *args:
    cargo build --release --locked
    ./target/release/overseer "$@"

# Every check CI runs.
check:
    cargo fmt --all --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo nextest run --locked --profile ci
    cargo shear
    cargo deny check
    typos
