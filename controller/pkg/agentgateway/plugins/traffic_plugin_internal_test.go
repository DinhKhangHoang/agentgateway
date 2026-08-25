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
