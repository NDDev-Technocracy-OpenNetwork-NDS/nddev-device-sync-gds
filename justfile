set shell := ["bash", "-euo", "pipefail", "-c"]

check:
    cargo fmt -- --check
    cargo clippy --locked --all-targets --all-features -- -D warnings
    cargo test --locked --all-features

provider-check:
    cargo test --locked --test provider --test isolation

catalog-check:
    cargo clippy --locked --all-targets --features github-catalog -- -D warnings
    cargo test --locked --features github-catalog

commit-check:
    cargo clippy --locked --all-targets --features catalog-runtime,commit-sync -- -D warnings
    cargo test --locked --features catalog-runtime,commit-sync

github-negative-check:
    cargo test --locked --features github-catalog --test github_provider -- --ignored
