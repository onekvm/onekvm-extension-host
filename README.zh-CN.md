# onekvm-extension-host

[English](README.md) | 简体中文

面向 OneKVM 扩展的低内存 D-Bus 生命周期宿主（libsystemd sd-bus）。同一二进制
也是扩展管理 CLI：无参数时认领 `org.onekvm.ExtensionHost1`；`catalog` /
`install` / `enable` 在进程内执行。CLI 入口是 `onekvm-extension`（指向本
二进制的符号链接）。Core 调用 `/usr/bin/onekvm-extension-host`。滚动升级
期间仍提供 `onekvm-extension-manager`。

## 编译和测试

```sh
cargo build --release --locked
cargo test --locked
```

OpenEmbedded 配方仍保留在 `meta-onekvm` layer。

## 许可证

MIT，详见 [LICENSE](LICENSE)。
