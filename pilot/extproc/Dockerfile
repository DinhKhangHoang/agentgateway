# The image this WOULD be shipped as, once a registry accepting pushes exists.
#
# Not used today: vcr.vngcloud.vn/60108-backend-worker/portal-external/dev/ is
# pull-only for the account available here (`docker login` succeeds, `docker
# push` returns 401), so pilot/17-extproc.yaml mounts these same sources from a
# ConfigMap into golang:1.26-alpine and runs `go run .` instead. Committed so
# the switch to a built image is a one-line change to `image:` plus deleting the
# `command:`/`workingDir:` and the src volume — the code that runs is identical
# either way.
#
# Builder pinned to 1.26 to match the local toolchain and the deployed image.
FROM golang:1.26 AS build
WORKDIR /src
# Copy the manifests first so `go mod download` is cached independently of the
# sources; unlike the adapter and the registry stub, this module has real
# dependencies (go-control-plane, grpc) and downloading them is the slow step.
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -o /pilot-extproc .

FROM gcr.io/distroless/static-debian12
COPY --from=build /pilot-extproc /pilot-extproc
# gRPC (ext_proc) and the health/metrics port. See pilot/17-extproc.yaml.
EXPOSE 50051 8081
ENTRYPOINT ["/pilot-extproc"]
