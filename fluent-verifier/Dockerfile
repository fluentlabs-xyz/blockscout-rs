FROM ghcr.io/blockscout/services-base:latest AS chef

FROM chef AS plan
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS cache
COPY --from=plan /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

FROM chef AS build

COPY . .
COPY --from=cache /app/target target
COPY --from=cache $CARGO_HOME $CARGO_HOME
RUN cargo build --release

FROM ghcr.io/blockscout/services-base:latest AS run

WORKDIR /app
ENV APP_USER=app

RUN groupadd --system $APP_USER \
    && useradd --system --gid $APP_USER --create-home $APP_USER

COPY --from=build /app/target/release/fluent-verifier-server /app/fluent-verifier-server
# Change directory access for app user
RUN chown -R $APP_USER:$APP_USER /app
USER $APP_USER

CMD ["./fluent-verifier-server"]
