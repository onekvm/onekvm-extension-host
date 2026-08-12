# onekvm-extension-host

[English](README.md) | 简体中文

面向 OneKVM 扩展的低内存 D-Bus 进程宿主。插件管理器使用它启动扩展进程并
分发生命周期操作，从而避免增加另一个占用较大的运行时服务。

## 编译和测试

```sh
cargo build --release --locked
cargo test --locked
```

OpenEmbedded 配方仍保留在 `meta-onekvm` layer。

## 许可证

MIT，详见 [LICENSE](LICENSE)。
