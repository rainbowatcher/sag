# 部署

## 构建

```bash
cargo build --release
```

编译后的二进制文件位于 `target/release/sag`。

## 运行

```bash
./target/release/sag --config /path/to/gateway.toml
```

## Docker

```dockerfile
FROM rust:alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY . .
RUN cargo build --release

FROM alpine
COPY --from=build /app/target/release/sag /usr/local/bin/sag
COPY gateway.toml /etc/sag/
EXPOSE 3000
CMD ["sag", "--config", "/etc/sag/gateway.toml"]
```

## 环境变量

| 变量 | 说明 |
|----------|------|
| `RUST_LOG` | Tracing 过滤器（例如 `info,sag=debug`） |
| `SAG_CONFIG` | 配置文件路径（`--config` 的替代方式） |

## 优雅关闭

SAG 处理 `SIGINT` 和 `SIGTERM` 信号以实现优雅关闭，允许正在处理的请求完成。
