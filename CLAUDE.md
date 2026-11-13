# irc-rtlog-viewer

## Tests use `--profile quick`, not `--release`

The release profile is fat LTO with one codegen unit and `opt-level = "s"` for the OpenWrt binary; a test rebuild under it takes about 35 s. `cargo test --profile quick` (opt-level 1, no LTO, incremental) runs the whole suite in a few seconds, and the pre-commit hook uses it. `make release` and `cargo build --release` stay on the size-optimised profile.
