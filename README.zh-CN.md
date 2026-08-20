# onekvm-extension-host

[English](README.md) | 简体中文

面向 OneKVM 扩展的低内存 D-Bus 生命周期宿主（libsystemd sd-bus）。它实现
`org.onekvm.ExtensionHost1`，并按扩展拉起 `--plugin-helper`。安装/卸载仍由
`onekvm-extension-manager` 负责。

## 编译和测试

```sh
cargo build --release --locked
cargo test --locked
```

OpenEmbedded 配方仍保留在 `meta-onekvm` layer。

## 许可证

MIT，详见 [LICENSE](LICENSE)。
