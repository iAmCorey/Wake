# Wake 网页版镜像。在仓库根目录构建:
#   docker build -f crates/wake-web/Dockerfile -t wake-web .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/wake-core crates/wake-core
COPY crates/wake-web crates/wake-web
# 网页版内嵌桌面版的图标与品牌图
COPY crates/wake/assets crates/wake/assets
# 桌面版(gpui,依赖树巨大)不进镜像:从工作区成员里摘掉,只编 wake-core + wake-web
RUN sed -i 's#"crates/wake", ##' Cargo.toml && cargo build --release -p wake-web

FROM debian:bookworm-slim
# 统计页按 TZ 划分"今天",slim 镜像默认不带时区库
RUN apt-get update && apt-get install -y --no-install-recommends tzdata && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/wake-web /usr/local/bin/wake-web
ENV WAKE_DATA=/data WAKE_INDEX=/index WAKE_LISTEN=0.0.0.0:8080
EXPOSE 8080
VOLUME ["/index"]
CMD ["wake-web"]
