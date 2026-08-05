# Homepage 导航主页

极简 NAS 导航页：添加/存储站点，点击图标跳转。Rust 微服务（axum）+ 单文件前端，常驻内存 ~3-5 MB。

## 功能

- **两种添加模式**，按模式自动归到对应页：
  - **域名模式**：输子域名自动拼 `scheme://子域.基础域:端口`，或直接粘完整 URL——智能识别。
  - **本地 IP 模式**：协议下拉（http/https，默认 http）+ `IP:端口`；粘贴带 `https://`/`http://` 前缀会自动切换并剥离。
- **原始高清图标**：后端解析目标首页 `<link>`，按矢量 SVG > apple-touch-icon > 大尺寸 PNG 取最优；LAN 自签名 HTTPS 站点自动放宽证书校验。
- **本地图片资源库**：把图片放到 `data/assets/`，前端编辑时从缩略图网格选图作为图标——不依赖外网、确定可控。
- **布局可调**（仅存本设备）：每行 4/5/6 个图标、盒子高度滑块；移动端"盒子"布局（单手友好），PC 宽屏自适应。
- **编辑模式**：长按磁贴拖拽排序、删除角标。
- **换页**：手机左右滑动；PC 两侧低存在感箭头 + 底部圆点可点击。
- **安全**：SSRF 防护 + cookie 隔离 + 前端 XSS 双层 sanitize + 路径穿越防护。
- **独立 Compose**：可独立部署，经反代接入现有认证体系。

## 快速开始

```bash
# 1. 准备环境
cp .env.example .env
install -d -o 1000 -g 1000 -m 0750 ./data ./data/icons ./data/assets

# 2. 启动
docker compose up -d

# 3. 访问 http://localhost:8080
```

> 想用自己的图片做站点图标？丢到 `data/assets/`，编辑站点时点「资源库」选即可。

## 构建

两种方式，任选其一：

### GitHub Actions（推荐）
推送 `v*` 格式的 tag 自动触发构建，推送到 GHCR：
```bash
git tag v0.1.0
git push origin v0.1.0
```
也可在 GitHub 仓库手动触发：Actions → release → Run workflow（产 `:edge` tag）。

### 本地 Docker
```bash
docker build --platform linux/amd64 -t local/homepage:latest -f Dockerfile .
```
产物镜像名 `local/homepage:latest`，`docker compose` 的 `image:` 字段指向它即可使用。

### 架构
- **构建环境**：`rust:1.83-alpine`（musl 工具链，静态链接）
- **运行环境**：`scratch` 空镜像，仅含一个静态二进制
- **内存占用**：常驻 ~3-5 MB
- **前端**：HTML+CSS+JS 单文件，编译期 `include_str!` 内嵌进二进制，无运行时依赖

## 修改页面（无需重编译）

挂载 `data/index.html` 会优先于内嵌版本生效，直接编辑后重启容器即可：
```bash
vim data/index.html
docker compose restart homepage
```

## 配置

参考 `.env.example`，核心变量：

| 变量 | 说明 |
|---|---|
| `HOMEPAGE_IMAGE_OWNER` | GHCR 镜像所属 GitHub 用户（小写），用于拼镜像 tag |
| `HOMEPAGE_COOKIE_DOMAIN` | cookie 转发域名，用于向目标站点传递登录态取高清图标 |
| `HOMEPAGE_LAN_CIDRS` | 内网段，如 `192.168.0.0/16`，SSRF 放行列表 |

## 许可证

MIT
