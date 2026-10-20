# onekvm-extension-host

环境包使用 `kind: environment`，安装后即可使用，无需启用，不运行常驻服务。
`kernel_modules` 声明模块依赖，不表示预加载；网络设置只在配置对应接口时加载
所需模块。旧网络环境包升级时清除启用状态并保留设置，不卸载已经加载的模块。
宿主启动和系统升级后的恢复流程也不会预加载环境包的模块。
创建网络接口时，Core 通过仅允许 root 调用的 `LoadNetworkModule(st)` D-Bus 方法
请求白名单内的单个模块，保留的 revision 参数为 0；CLI 入口为
`onekvm-extension load-network-module <module>`。Core 无需获取 `CAP_SYS_MODULE`。
启动准备阶段的 CLI 可直接加载，未启用的 VPN 连接不请求模块。

[English](README.md) | 简体中文

面向 OneKVM 扩展的低内存 D-Bus 生命周期宿主（libsystemd sd-bus）。同一二进制
也是扩展管理 CLI：无参数时认领 `org.onekvm.ExtensionHost1`；`catalog` /
`install` / `enable` 在进程内执行。CLI 入口是 `onekvm-extension`（指向本
二进制的符号链接）。Core 调用 `/usr/bin/onekvm-extension-host`。滚动升级
期间仍提供 `onekvm-extension-manager`。

## 插件市场大小

下载大小按当前设备的架构优先级，递归统计插件的 `Pre-Depends`、`Depends` 和
`Recommends`，与 opkg 默认安装策略一致。统计保留版本约束、备选依赖和虚拟包提供者，
共享包只计一次；已安装且满足条件的依赖不增加下载量或空间开销，需要升级的包仍计入。
安装大小来自软件源中解压后的载荷大小，已安装插件仍显示自身占用。缓存同时跟踪
opkg 安装状态、软件源和架构配置。

## 编译和测试

```sh
cargo build --release --locked
cargo test --locked
```

组件自有 OpenEmbedded 配方位于 `oe-recipes`。它同时安装 systemd
和 D-Bus 策略。旧 `onekvm-plugin-manager` 包由本包替换并提供升级兼容，其命令名仍
是指向这个 Rust 二进制的符号链接。

## 直接方法与动态目录

`detail <id>` 命令（Core 的 `GET /api/extensions/{id}` 也使用该接口）返回插件声明的
`permissions` 和 `kernel_modules`。`package_dependencies` 读取所属已安装软件包的
直接 `Depends` 依赖并保留版本约束；没有匹配当前插件版本的安装记录时为 `null`，未声明依赖时为空数组。
插件文件会在 A/B 系统槽切换后保留。当前槽缺少匹配记录时，宿主可从另一槽读取
与活动插件版本一致的记录；当前槽的匹配记录始终优先，包括未声明依赖的记录。
验证通过的依赖记录按软件包和插件版本保存在 `/data/pkg-state/extensions`，在后续
系统更新后仍可读取。无法写入保存目录时，已确认的依赖仍会正常返回。
可安装插件的依赖来自软件源元数据；系统插件通过 opkg 文件列表查找其注册文件所属的软件包。

API v2 manifest 可以将 `methods` 声明为数组。方法可以提供插件包内的相对
`command` 和带类型的 `args`；标记为 `"secret": true` 的参数必须是字符串，
宿主会从 JSON 请求中校验，但不会追加到进程参数。完整请求通过标准输入传给
命令。纯命令扩展不需要常驻 entrypoint，也不需要健康检查。命令方法也可以
搭配 legacy `entrypoint` 守护进程，但不要同时声明 `controller`。

manifest 中的 `kernel_modules` 由具备权限的 D-Bus 宿主在启用和恢复时加载，
纯命令环境插件也遵循此规则。管理 CLI 不自行加载模块，Core 等调用方不需要
`CAP_SYS_MODULE`。

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

## 按连接启动

入口进程型扩展可声明 `socket_activation`，用 `address_setting` 和
`port_setting` 指定配置中的监听地址和 TCP 端口。地址必须是 IPv4/IPv6
数字地址，端口为 1–65535 的整数，并声明 `network` 权限。

宿主创建同名的 `.socket` / `.service` 瞬态单元，使用 `Accept=no`。
进程通过 `LISTEN_PID` / `LISTEN_FDS` 接收 fd 3 的监听 Socket；成功的
空闲退出后，systemd 会在下次连接时重新启动进程。监听前配置和 Core
授权已经准备好，进程继续使用宿主的沙箱和资源限制。禁用、升级、配置
更改时先停止监听，再停止服务；两者都随 Core 生命周期关闭。后台把
正在监听的扩展视为可用。此声明不与 controller 或 `settings_reload: "watch"`
组合。完整字段例子见 [English README](README.md#socket-activation)。

## 许可证

MIT，详见 [LICENSE](LICENSE)。

插件升级事务、失败恢复、通用贡献项通知与实机验收见
[扩展生命周期](docs/extension-lifecycle.md)。
