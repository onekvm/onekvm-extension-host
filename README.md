# onekvm-extension-host

English | [简体中文](README.zh-CN.md)

A low-memory D-Bus process host for OneKVM extensions. The plugin manager uses
it to launch extension processes and dispatch lifecycle operations without
adding a second large runtime service.

## Build and test

```sh
cargo build --release --locked
cargo test --locked
```

The OpenEmbedded recipe remains in the `meta-onekvm` layer.

## License

MIT. See [LICENSE](LICENSE).
