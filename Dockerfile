FROM rust:bookworm AS build

WORKDIR /src
COPY . .
RUN cargo +stable build --release -p temporal-trivia-web \
    --bin phone_api --bin phone_worker

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/phone_api /usr/local/bin/phone_api
COPY --from=build /src/target/release/phone_worker /usr/local/bin/phone_worker

ENV PORT=8080
CMD ["/usr/local/bin/phone_api"]
