# 多阶段构建：rust:alpine(musl) 编译 → scratch 静态运行。
# 目标：最小镜像 + 最小内存。最终镜像只含一个静态二进制 + /data 挂载点。
# 只在 GitHub Actions 中由 docker/build-push-action 构建。
# NAS 架构为 linux/amd64，CI 也以 amd64 构建。

# ---------- 构建阶段 ----------
# rust:alpine 默认以 musl 工具链编译，产出即为静态二进制，无需额外 target。
FROM docker.io/library/rust:1.95-alpine AS builder

# musl 链接所需的 libc-dev（alpine 基础镜像已含 musl，这里补 headers）。
RUN apk add --no-cache musl-dev

WORKDIR /build

# ---- 依赖缓存层 ----
# 用一个空 main.rs 先单独编译依赖；后续源码改动不会重编译依赖，大幅加速 CI。
COPY Cargo.toml ./
RUN mkdir -p src && echo 'fn main() { println!("dummy"); }' > src/main.rs && \
    cargo build --release && \
    # 保留依赖产物，只删除 dummy 的 homepage 二进制与它的指纹，
    # 真实编译时 cargo 会重建二进制但复用所有依赖。
    rm -f target/release/homepage && \
    find target/release -name 'homepage-*' -exec rm -f {} +

# ---- 真实编译 ----
# frontend-dist/index.html 由 include_str! 在编译期嵌入，必须存在。
COPY src ./src
COPY frontend-dist ./frontend-dist
RUN cargo build --release && cp target/release/homepage /homepage

# ---------- 运行阶段 ----------
FROM scratch

# 非 root：UID/GID 1000（与 Caddy 容器一致，便于同卷权限）。
# scratch 无 /etc/passwd；USER 仅需数字 UID:GID。
COPY --from=builder /homepage /homepage

# /data 由 compose 以 bind mount 提供；VOLUME 仅作文档。
VOLUME ["/data"]

EXPOSE 8080

# 根文件系统只读友好：二进制与首页内嵌进镜像，运行时只写挂载的 /data。
USER 1000:1000

ENTRYPOINT ["/homepage"]
