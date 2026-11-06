FROM rust:1.99-alpine AS build
RUN apk add --no-cache musl-dev cmake clang perl make
WORKDIR /src
# Dependency layer: compile against a stub so source edits don't rebuild the world.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && (cargo build --release --locked || true) && rm -rf src
COPY migrations migrations
COPY templates templates
COPY static static
COPY src src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM gcr.io/distroless/static-debian13:nonroot
COPY --from=build /src/target/release/mealier /mealier
ENV MEALIER_DATA_DIR=/data BIND_ADDR=0.0.0.0 PORT=9000
VOLUME /data
USER nonroot
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["/mealier","healthcheck"]
ENTRYPOINT ["/mealier"]
