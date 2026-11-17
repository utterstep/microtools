# microtools

Small, single-purpose tools I run on my own machines. One directory per tool, each with
its own README.

| Tool | What it does | Language |
| --- | --- | --- |
| [`hey-forward`](hey-forward) | Daemon that forwards [HEY](https://www.hey.com) mail sent to chosen `+aliases` on to other addresses | Rust |

## Layout

- Each tool lives in a top-level directory of its own and documents itself there.
- Rust is the default. Rust tools are members of the Cargo workspace at the root and
  share its lockfile, lints and release profile.
- A tool in another language is simply a directory that builds its own way.

## Building

```sh
cargo build --release                  # every Rust tool
cargo build --release -p hey-forward   # just one
cargo test
```

Binaries land in `target/release/`.

## Adding a tool

`cargo new <name>` at the root joins it to the workspace. Point its `Cargo.toml` at the
shared settings (`edition.workspace = true` and friends, `[lints] workspace = true`), give
it a README, and add a row to the table above.

## License

[MIT](LICENSE)
