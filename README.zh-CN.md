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

组件自有 OpenEmbedded 配方位于 `oe-recipes`。它同时安装 systemd
和 D-Bus 策略。旧 `onekvm-plugin-manager` 包由本包替换并提供升级兼容，其命令名仍
是指向这个 Rust 二进制的符号链接。

## 直接方法与动态目录

API v2 manifest 可以将 `methods` 声明为数组。方法可以提供插件包内的相对
`command` 和带类型的 `args`；标记为 `"secret": true` 的参数必须是字符串，
宿主会从 JSON 请求中校验，但不会追加到进程参数。完整请求通过标准输入传给
命令。纯命令扩展不需要常驻 entrypoint，也不需要健康检查。命令方法也可以
搭配 legacy `entrypoint` 守护进程，但不要同时声明 `controller`。

已启用的扩展可以原子写入
`$ONEKVM_EXTENSION_DATA/directories-output.json`，动态发布运行时目录：

```json
{
  "directories": [{
    "id": "gateway-0011223344556677",
    "name": "Lab NAS",
    "kind": "virtual-media",
    "role": "gateway",
    "path": "gateways/gateway-0011223344556677",
    "location": "data",
    "writable": false
  }]
}
```

动态项会和 manifest 中的静态 `directories` 合并，只在插件启用时可见。动态项
必须使用安全相对路径和 `location: "data"`，不能与静态 ID 重复；最多 32 项，
输出文件最大 64 KiB。写入时应使用原子 rename，避免 catalog 读到不完整 JSON。可选 `name` 用于显示用户填写的目录名称
（最多 255 UTF-8 字节、不能含控制字符）；`id` 仍作为稳定标识。

## 许可证

MIT，详见 [LICENSE](LICENSE)。
