# Homepage 导航主页

极简 NAS 导航页：添加/存储站点，点击图标跳转。Rust 微服务（axum）+ 单文件前端，常驻内存 ~3-5 MB。

## 功能

- **两种添加模式**，按模式自动归到对应页：
  - **域名模式**：输子域名自动拼 `scheme://子域.基础域:端口`，或直接粘完整 URL——智能识别。
  - **本地 IP 模式**：协议下拉（http/https，默认 http）+ `IP:端口`；粘贴带 `https://`/`http://` 前缀会自动切换并剥离。
- **原始高清图标**：后端解析目标首页 `<link>`，按 矢量 SVG > apple-touch-icon > 大尺寸 PNG 取最优；LAN 自签名 HTTPS 站点自动放宽证书校验。
- **本地图片资源库**：把图片放到 `data/assets/`，前端编辑时从缩略图网格选图作为图标——不依赖外网、确定可控。
- **布局可调**（仅存本设备）：每行 4/5/6 个图标、盒子高度滑块；移动端"盒子"布局（标题顶部、应用下沉到中下部，单手友好），PC 宽屏自适应。
- **编辑模式**：长按磁贴拖拽排序、删除角标、右上角「完成」退出。
- **换页**：手机左右滑动；PC 两侧低存在感箭头 + 底部圆点可点击。
- **安全**：Authelia 双因子门禁；SSRF 防护 + cookie 隔离 + 前端 XSS 双层 sanitize + 路径穿越防护。
- **独立 Compose**：与 caddy-authelia 解耦，复用其 `caddy-authelia-edge` 网络经 Caddy 反代。

## 文档

| 文档 | 内容 |
|---|---|
| [AGENTS.md](AGENTS.md) | **AI agent 项目级指导**（架构、约定、常见任务） |
| [构建与发布](docs/构建与发布.md) | **两种构建工作流**：GitHub Actions → GHCR（零本地依赖） / 本地自编译（离线可用） |
| [部署指南](docs/部署指南.md) | NAS 部署步骤、`.env`、数据权限、启动顺序、反代接线、改页面/图标/资源库 |
| [设计与安全](docs/设计与安全.md) | 架构、图标机制、数据模型、API、安全设计（SSRF/cookie/XSS/TLS） |
| [本地编译测试(Win11)](docs/本地编译测试(Win11).md) | Win11 原生 `cargo` 编译 + 功能验证 |
| [项目总结与设计决策](docs/项目总结与设计决策.md) | 需求演进 + 「为什么这样」的决策记录 |

## 快速开始

```bash
# 1. 镜像就绪（GHCR 拉取 或 本地构建，见 docs/构建与发布.md）
cp .env.example .env          # 填 HOMEPAGE_IMAGE_OWNER / COOKIE_DOMAIN / LAN_CIDRS
install -d -o 1000 -g 1000 -m 0750 ./data ./data/icons ./data/assets

# 2. 先确保 caddy-authelia 已运行（拥有 edge 网络），再起本服务
docker compose up -d

# 3. 访问 https://home.nas.lan:8443（Authelia 登录后）
```

> 想用自己的图片做站点图标？丢到 `data/assets/`，编辑站点时点「资源库」选即可。
> 与 caddy-authelia 栈的关系、启动顺序详见 [部署指南](docs/部署指南.md)。
