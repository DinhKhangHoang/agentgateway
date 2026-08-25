package plugins

import (
	"testing"

	"k8s.io/apimachinery/pkg/api/resource"
	"k8s.io/apimachinery/pkg/types"
	gwv1 "sigs.k8s.io/gateway-api/apis/v1"

	"github.com/agentgateway/agentgateway/controller/api/v1alpha1/agentgateway"
)

func byteSize(t *testing.T, s string) *agentgateway.ByteSize {
	t.Helper()
	q := resource.MustParse(s)
	return &agentgateway.ByteSize{Value: &q}
}

func retryWith(size *agentgateway.ByteSize) *agentgateway.Retry {
	attempts := 2
	return &agentgateway.Retry{
		HTTPRouteRetry: &gwv1.HTTPRouteRetry{Attempts: &attempts},
		MaxReplayBytes: size,
	}
}

func TestProcessRetriesPolicyTranslatesMaxReplayBytes(t *testing.T) {
	// Binary suffix: 50Mi is 52428800, not 50000000. The distinction matters
	// because `50M` would be a different (decimal) number.
	got, err := processRetriesPolicy(retryWith(byteSize(t, "50Mi")), "base", types.NamespacedName{Name: "n", Namespace: "ns"})
	if err != nil {
		t.Fatalf("processRetriesPolicy: %v", err)
	}
	if v := got.GetTraffic().GetRetry().GetMaxReplayBytes(); v != 52_428_800 {
		t.Fatalf("MaxReplayBytes = %d, want 52428800", v)
	}
}

func TestProcessRetriesPolicyTranslatesDecimalMaxReplayBytes(t *testing.T) {
	// The decimal suffix is accepted too, and means exactly what Kubernetes says
	// it means: 50M is 50000000, which is 2428800 fewer bytes than 50Mi.
	got, err := processRetriesPolicy(retryWith(byteSize(t, "50M")), "base", types.NamespacedName{Name: "n", Namespace: "ns"})
	if err != nil {
		t.Fatalf("processRetriesPolicy: %v", err)
	}
	if v := got.GetTraffic().GetRetry().GetMaxReplayBytes(); v != 50_000_000 {
		t.Fatalf("MaxReplayBytes = %d, want 50000000", v)
	}
}

func TestProcessRetriesPolicyAcceptsBoundaryMaxReplayBytes(t *testing.T) {
	for _, tc := range []struct {
		in   string
		want uint32
	}{
		{"1Ki", 1024},
		{"100Mi", 104_857_600},
		// A fractional quantity is idiomatic Kubernetes and 1.5Mi is well inside the
		// range, but AsInt64() reports every non-integer quantity unrepresentable --
		// gating on it discarded this value and silently reverted the cap to 64 KiB.
		{"1.5Mi", 1_572_864},
	} {
		got, err := processRetriesPolicy(retryWith(byteSize(t, tc.in)), "base", types.NamespacedName{Name: "n", Namespace: "ns"})
		if err != nil {
			t.Fatalf("processRetriesPolicy(%s): %v", tc.in, err)
		}
		if v := got.GetTraffic().GetRetry().GetMaxReplayBytes(); v != tc.want {
			t.Fatalf("MaxReplayBytes(%s) = %d, want %d", tc.in, v, tc.want)
		}
	}
}

func TestProcessRetriesPolicyOmitsMaxReplayBytesWhenUnset(t *testing.T) {
	got, err := processRetriesPolicy(retryWith(nil), "base", types.NamespacedName{Name: "n", Namespace: "ns"})
	if err != nil {
		t.Fatalf("processRetriesPolicy: %v", err)
	}
	if got.GetTraffic().GetRetry().GetMaxReplayBytes() != 0 {
		t.Fatal("MaxReplayBytes should stay 0 so the dataplane applies its default")
	}
}

// A rejected quantity must leave the rest of the retry policy intact. Returning a
// nil policy here would silently disable retries entirely -- the exact failure this
// field exists to prevent -- so every rejection case asserts the policy survives.
func TestProcessRetriesPolicyKeepsPolicyWhenMaxReplayBytesIsRejected(t *testing.T) {
	cases := map[string]string{
		"negative":      "-1",
		"zero":          "0",
		"below floor":   "512",
		"above ceiling": "200Mi",
		// CmpInt64 compares these exactly, so the range check itself excludes them.
		// They are listed because the lossy int64 conversions do not: through
		// Quantity.Value(), which returns the low 64 bits rather than saturating,
		// "1e30" reads back as 0 and the 20-digit literal as 65536 -- an in-range
		// number. "16Ei" saturates Value() to MaxInt64. Any of them silently
		// becoming a valid cap is the regression these rows exist to catch.
		"too large for int64":              "1e30",
		"wraps to in-range in low 64 bits": "18446744073709617152",
		"saturates int64":                  "16Ei",
	}
	for name, in := range cases {
		t.Run(name, func(t *testing.T) {
			got, err := processRetriesPolicy(retryWith(byteSize(t, in)), "base", types.NamespacedName{Name: "n", Namespace: "ns"})
			if err == nil {
				t.Fatalf("expected an error reporting the out-of-range quantity %q", in)
			}
			if got == nil {
				t.Fatal("policy must still be returned so the remaining retry config applies")
			}
			if v := got.GetTraffic().GetRetry().GetMaxReplayBytes(); v != 0 {
				t.Fatalf("MaxReplayBytes = %d, want 0 so the dataplane default applies", v)
			}
			if v := got.GetTraffic().GetRetry().GetAttempts(); v != 2 {
				t.Fatalf("Attempts = %d, want 2 -- other fields must survive", v)
			}
		})
	}
}

// An unparsable quantity is dropped by ByteSize.UnmarshalJSON (which warns and
// leaves Value nil) rather than reaching here, so a nil Value must be treated as
// unset instead of dereferenced.
func TestProcessRetriesPolicyTreatsEmptyByteSizeAsUnset(t *testing.T) {
	got, err := processRetriesPolicy(retryWith(&agentgateway.ByteSize{}), "base", types.NamespacedName{Name: "n", Namespace: "ns"})
	if err != nil {
		t.Fatalf("processRetriesPolicy: %v", err)
	}
	if got.GetTraffic().GetRetry().GetMaxReplayBytes() != 0 {
		t.Fatal("MaxReplayBytes should stay 0 so the dataplane applies its default")
	}
}
