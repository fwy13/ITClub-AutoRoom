# syntax=docker/dockerfile:1
FROM node:22-alpine AS frontend
WORKDIR /web
COPY web/package*.json ./
RUN npm install --no-audit --no-fund
COPY web/ ./
RUN npm run build

FROM rust:1-bookworm AS backend
WORKDIR /source
COPY server/ ./server/
COPY fixtures/ ./fixtures/
RUN cd server && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/* \
  && mkdir -p /data /app/public && chown -R 10001:10001 /data /app
WORKDIR /app
COPY --from=backend /source/server/target/release/itclub-autoroom /app/itclub-autoroom
COPY --from=frontend /web/dist/ /app/public/
USER 10001:10001
ENV BIND_ADDR=0.0.0.0:3000 DATABASE_URL=sqlite:///data/reservations.db
EXPOSE 3000
CMD ["/app/itclub-autoroom"]
