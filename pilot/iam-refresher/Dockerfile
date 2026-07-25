# Builder must be >= the toolchain in go.mod (go 1.26.0), or the build fails on
# a go-directive mismatch.
FROM golang:1.26 AS build
WORKDIR /src
COPY . .
RUN CGO_ENABLED=0 go build -o /iam-refresher .

FROM gcr.io/distroless/static-debian12
COPY --from=build /iam-refresher /iam-refresher
ENTRYPOINT ["/iam-refresher"]
