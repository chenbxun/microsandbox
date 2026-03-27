# OverlayBD

microsandbox 支持使用 [OverlayBD](https://github.com/containerd/overlaybd) 镜像作为 microVM 的 rootfs 存储后端。OverlayBD 是一种基于块设备的容器镜像格式，支持按需加载（lazy pulling），可以显著加速容器启动。

---

## 存储后端

microsandbox 目前支持以下 rootfs 存储后端：

| 后端 | 说明 |
|------|------|
| naive | 使用本地目录作为根文件系统 |
| overlayfs | 使用 overlayfs 联合文件系统挂载镜像层 |
| **overlaybd** | 使用 OverlayBD 块设备格式，支持按需加载 |

---

## 配置

要使用 OverlayBD 后端，只需将 `image` 设置为 OverlayBD 格式的镜像地址，程序会拉取 manifest 自动判断镜像类型。同时需要添加 `block_device` 字段来配置块设备参数。

### `block_device` 字段

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `size` | u64 | `256` | 块设备大小 / 可写层的 vsize，单位 GB |
| `filesystem` | String | `ext4` | 格式化块设备的文件系统类型。若设备已有文件系统则跳过格式化 |
| `sparse` | bool | `true` | 可写层是否使用稀疏文件 |

---

## 示例

### 基本用法

```yaml
sandboxes:
  redis:
    image: 'dadi-test-registry.cn-hangzhou.cr.aliyuncs.com/sample-v2/redis:20240403_containerd_accelerated'
    memory: 1024
    cpus: 1
    shell: /bin/sh
    scripts:
      start: echo "hello"
    block_device:
      size: 256
      filesystem: ext4
      sparse: true
```

### 最小配置

`block_device` 所有子字段均有默认值，可以只声明 `block_device` 并按需覆盖：

```yaml
sandboxes:
  minimal:
    image: 'registry.example.com/myimage:tag_accelerated'
    memory: 1024
    cpus: 1
    shell: /bin/sh
    scripts:
      start: echo "hello"
    block_device:
      size: 20
```

