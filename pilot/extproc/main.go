package main

import (
	"context"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	extprocv3 "github.com/envoyproxy/go-control-plane/envoy/service/ext_proc/v3"
	"google.golang.org/grpc"
	"google.golang.org/grpc/keepalive"
)

func main() {
	log := slog.New(slog.NewJSONHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelInfo}))
	slog.SetDefault(log)

	cfg, err := LoadConfig(os.Getenv)
	if err != nil {
		log.Error("refusing to start", "error", err.Error())
		os.Exit(1)
	}

	metrics := &Metrics{}
	proc := NewProcessor(cfg, NewHTTPPolicyClient(cfg.PolicyURL), log, metrics)

	grpcSrv := grpc.NewServer(
		// A chat completion can legitimately stream for minutes. Ping the peer
		// rather than assuming a quiet stream is dead.
		grpc.KeepaliveParams(keepalive.ServerParameters{
			Time:    30 * time.Second,
			Timeout: 10 * time.Second,
		}),
		grpc.KeepaliveEnforcementPolicy(keepalive.EnforcementPolicy{
			MinTime:             10 * time.Second,
			PermitWithoutStream: true,
		}),
		// Request bodies are bounded by MAX_REQUEST_BODY_BYTES on our side, but
		// the gateway frames each body chunk as one message; leave headroom.
		grpc.MaxRecvMsgSize(16*1024*1024),
		grpc.MaxSendMsgSize(16*1024*1024),
	)
	extprocv3.RegisterExternalProcessorServer(grpcSrv, proc)

	lis, err := net.Listen("tcp", ":"+cfg.GRPCPort)
	if err != nil {
		log.Error("cannot listen for gRPC", "port", cfg.GRPCPort, "error", err.Error())
		os.Exit(1)
	}

	// Health on a SEPARATE plain-HTTP port: kubelet probes must never be
	// confused with a real ext_proc stream, and a gRPC health probe would not
	// tell us the process can still serve HTTP to the plugin server.
	mux := http.NewServeMux()
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("ok\n"))
	})
	mux.HandleFunc("/metrics", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/plain; version=0.0.4")
		fmt.Fprintf(w, "# HELP pilot_extproc_streams_total ext_proc streams opened\n")
		fmt.Fprintf(w, "# TYPE pilot_extproc_streams_total counter\n")
		fmt.Fprintf(w, "pilot_extproc_streams_total %d\n", metrics.StreamsTotal.Load())
		for name, v := range map[string]int64{
			"pilot_extproc_guardrail_blocks_total":      metrics.GuardrailBlocks.Load(),
			"pilot_extproc_guardrail_unsupported_total": metrics.GuardrailUnsupported.Load(),
			"pilot_extproc_guardrail_skips_total":       metrics.GuardrailSkips.Load(),
			"pilot_extproc_check_failures_total":        metrics.CheckFailures.Load(),
			"pilot_extproc_usage_posted_total":          metrics.UsagePosted.Load(),
			"pilot_extproc_usage_failures_total":        metrics.UsageFailures.Load(),
			"pilot_extproc_usage_zero_delta_total":      metrics.UsageSkippedZeroDelta.Load(),
			"pilot_extproc_usage_missing_total":         metrics.UsageMissing.Load(),
			"pilot_extproc_response_chunks_total":       metrics.ResponseChunks.Load(),
		} {
			fmt.Fprintf(w, "# TYPE %s counter\n%s %d\n", name, name, v)
		}
	})
	healthSrv := &http.Server{
		Addr:              ":" + cfg.HealthPort,
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}
	go func() {
		if err := healthSrv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Error("health server stopped", "error", err.Error())
		}
	}()

	log.Info("pilot ext_proc listening",
		"grpc_port", cfg.GRPCPort,
		"health_port", cfg.HealthPort,
		"policy_server", cfg.PolicyURL,
		"estimated_tokens", cfg.EstimatedTokens,
		"unsupported_guardrail_action", string(cfg.UnsupportedGuardrailAction),
		"defer_request_headers_ack", cfg.DeferHeadersAck,
		"usage_timeout", cfg.UsageTimeout.String())

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		<-stop
		log.Info("shutting down")
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		_ = healthSrv.Shutdown(ctx)
		grpcSrv.GracefulStop()
	}()

	if err := grpcSrv.Serve(lis); err != nil {
		log.Error("gRPC server stopped", "error", err.Error())
		os.Exit(1)
	}
}
