# Builder pinned to 1.26 to match the local toolchain (go1.26.0) and satisfy
# the `go 1.25.0` directive in go.mod. golang:1.22 would fail the build.
FROM golang:1.26 AS build
WORKDIR /src
COPY . .
RUN CGO_ENABLED=0 go build -o /extauthz-adapter .

FROM gcr.io/distroless/static-debian12
COPY --from=build /extauthz-adapter /extauthz-adapter
ENTRYPOINT ["/extauthz-adapter"]
