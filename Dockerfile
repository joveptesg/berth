# Static musl build, scratch runtime image.
FROM rust:1.99.1-alpine AS build
RUN apk add --no-cache musl-dev ca-certificates
WORKDIR /src
COPY . .
RUN cargo build --release --locked --bin berth \
    && mkdir /data

FROM scratch
COPY --from=build /src/target/release/berth /berth
# Root certificates: S3, R2 and other storages are reached over https.
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /data /data
ENV BERTH_DATA_DIR=/data
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["/berth"]
CMD ["serve"]