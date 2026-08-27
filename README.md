# onekvm-extension-host

English | [简体中文](README.zh-CN.md)

A low-memory D-Bus process host for OneKVM extensions. The same binary is the
extension manager CLI: no arguments start `org.onekvm.ExtensionHost1`, while
`catalog` / `install` / `enable` and the rest of the former
`onekvm-extension-manager` commands run in-process. Core calls
`/usr/bin/onekvm-extension-host`. A compatibility symlink
`onekvm-extension-manager` remains for rolling upgrades.

## Build and test

```sh
cargo build --release --locked
cargo test --locked
```

The OpenEmbedded recipe remains in the `meta-onekvm` layer.

## License

MIT. See [LICENSE](LICENSE).
