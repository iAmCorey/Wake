# Wake 网页版(部署到 NAS)

把几台电脑的会话归档集中到 NAS 上,在浏览器里浏览、搜索、阅读、导出。只读:不会改动
任何会话文件。

```
电脑 A 的归档 ──┐
电脑 B 的归档 ──┼─同步─► NAS 共享文件夹 wake-data/
电脑 C 的归档 ──┘          ├── macbook/     ← 一台电脑一个子文件夹,名字就是机器名
                           ├── imac/
                           └── work-pc/
                                  │
                           wake-web 容器(只读挂载)──► 浏览器
```

## 1. 每台电脑:把归档同步到 NAS

每台电脑装桌面版 Wake(带会话归档的版本)。归档目录在 **设置 → 数据 → 会话归档**
里能看到,默认是 `~/Library/Application Support/wake/vault`,也可以点「更改…」换位置。

用 NAS 的同步客户端,把这个**归档目录**同步到 NAS 上 `wake-data/<机器名>/`。

- 机器名只能用字母、数字、`.`、`_`、`-`(不能有空格和中文),例如 `macbook`、`work-pc`。
- 同步后 NAS 上应该是 `wake-data/macbook/.claude/projects/…` 和
  `wake-data/macbook/.codex/sessions/…` 这样的结构。
- **`.claude`、`.codex` 是隐藏文件夹**。请确认同步客户端没有跳过隐藏文件,否则 NAS 上
  会是空的。
- 同步方向选「电脑 → NAS」的单向上传即可。归档本身只增不减,电脑上删了会话,归档和
  NAS 上都还在。

## 2. NAS:启动容器

镜像由仓库的 `web-image` 工作流构建(`ghcr.io/<你的 GitHub 用户名>/wake-web:latest`,
同时支持 Intel 和 ARM 机型)。也可以在仓库根目录自己构建:

```bash
docker build -f crates/wake-web/Dockerfile -t wake-web .
```

在 NAS 的 Docker 应用里新建项目,用 [`crates/wake-web/docker-compose.yml`](../crates/wake-web/docker-compose.yml),
改两处:

| 项 | 说明 |
|---|---|
| `/volume1/wake-data:/data:ro` | 左边换成 NAS 上 `wake-data` 共享文件夹的实际路径 |
| `8080:8080` | 左边是 NAS 上的端口,被占用就换 |

启动后在浏览器打开 `http://<NAS 地址>:8080`,不用登录。界面与桌面版一致:侧栏按机器、
Agent、项目筛选,`⌘K` / `Ctrl K` 全文搜索,可收藏、导出 Markdown。

## 3. 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `WAKE_PASSWORD` | 无 | 访问密码。默认不设、不用登录;设了才会出现登录框 |
| `WAKE_DATA` | `/data` | 数据根,每个子文件夹一台机器 |
| `WAKE_INDEX` | `/index` | 索引目录,可随时删掉重建 |
| `WAKE_LISTEN` | `0.0.0.0:8080` | 监听地址 |
| `WAKE_SCAN_SECS` | `30` | 多久检查一次新会话(秒) |

## 安全

会话里常有代码、路径,有时还有密钥。

- 默认**没有登录、不带 HTTPS**:能连到这个端口的人都能看到全部会话。在家里局域网用
  没问题。
- 从外网访问请走 NAS 自带的远程访问或 VPN,或者放在带 HTTPS 的反向代理后面;不要把
  这个端口直接映射到公网。需要多一道保护时设 `WAKE_PASSWORD`。

## 目前支持的工具

网页版能读的格式与桌面版的远程主机一致;但桌面版的**归档**目前只收 Claude Code 和
Codex,所以经归档同步上来的也就是这两家。
