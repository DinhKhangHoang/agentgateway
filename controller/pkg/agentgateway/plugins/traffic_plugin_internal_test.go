package plugins

import (
	"testing"

	"k8s.io/apimachinery/pkg/api/resource"
	"k8s.io/apimachinery/pkg/types"
	gwv1 "sigs.k8s.io/gateway-api/apis/v1"

	"github.com/agentgateway/agentgateway/controller/api/v1alpha1/agentgateway"
)

func TestProcessRetriesPolicyTranslatesMaxReplayBytes(t *testing.T) {
	attempts := 2
	maxBytes := resource.MustParse("50M")
	retry := &agentgateway.Retry{
		HTTPRouteRetry: &gwv1.HTTPRouteRetry{Attempts: &attempts},
		MaxReplayBytes: &maxBytes,
	}
	got, err := processRetriesPolicy(retry, "base", types.NamespacedName{Name: "n", Namespace: "ns"})
	if err != nil {
		t.Fatalf("processRetriesPolicy: %v", err)
	}
	spec := got.GetTraffic().GetRetry()
	if spec.GetMaxReplayBytes() != 50_000_000 {
		t.Fatalf("MaxReplayBytes = %d, want 50000000", spec.GetMaxReplayBytes())
	}
}

func TestProcessRetriesPolicyOmitsMaxReplayBytesWhenUnset(t *testing.T) {
	attempts := 2
	retry := &agentgateway.Retry{HTTPRouteRetry: &gwv1.HTTPRouteRetry{Attempts: &attempts}}
	got, err := processRetriesPolicy(retry, "base", types.NamespacedName{Name: "n", Namespace: "ns"})
	if err != nil {
		t.Fatalf("processRetriesPolicy: %v", err)
	}
	if got.GetTraffic().GetRetry().GetMaxReplayBytes() != 0 {
		t.Fatal("MaxReplayBytes should stay 0 so the dataplane applies its default")
	}
}

func TestProcessRetriesPolicyKeepsPolicyWhenMaxReplayBytesIsUnusable(t *testing.T) {
	// A quantity too large for int64 cannot be applied. The policy must still come
	// back with its other fields intact and MaxReplayBytes left at 0 (so the
	// dataplane applies its own default). Returning a nil policy here would
	// silently disable retries entirely -- the exact failure this field exists to
	// prevent.
	attempts := 2
	maxBytes := resource.MustParse("1e30")
	retry := &agentgateway.Retry{
		HTTPRouteRetry: &gwv1.HTTPRouteRetry{Attempts: &attempts},
		MaxReplayBytes: &maxBytes,
	}
	got, err := processRetriesPolicy(retry, "base", types.NamespacedName{Name: "n", Namespace: "ns"})
	if err == nil {
		t.Fatal("expected an error reporting the unusable quantity")
	}
	if got == nil {
		t.Fatal("policy must still be returned so the remaining retry config applies")
	}
	if got.GetTraffic().GetRetry().GetMaxReplayBytes() != 0 {
		t.Fatalf("MaxReplayBytes = %d, want 0 so the dataplane default applies",
			got.GetTraffic().GetRetry().GetMaxReplayBytes())
	}
	if got.GetTraffic().GetRetry().GetAttempts() != 2 {
		t.Fatalf("Attempts = %d, want 2 -- other fields must survive",
			got.GetTraffic().GetRetry().GetAttempts())
	}
}
