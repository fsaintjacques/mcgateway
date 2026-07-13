//go:build kind

package kind_test

import (
	"strings"
	"testing"

	mckind "github.com/fsaintjacques/mcgateway/go/internal/kind"
)

// TestNativeMergeDispatch proves libmcgateway_native.so loaded and is
// serving merge dispatch. A failure here almost always means the cdylib
// didn't make it into the image or couldn't be dlopened by the proxy.
func TestNativeMergeDispatch(t *testing.T) {
	s := newSuite(t)

	resp, err := mckind.McGetWithRetry(s.gwAddr, "__mcgw:names", 5)
	if err != nil {
		t.Fatalf("mg __mcgw:names v: %v", err)
	}

	// The three built-ins must always be present. WASM modules are
	// deliberately not asserted: in operator mode the UDF dir starts
	// empty and modules arrive via inline-wasm CRs, so which ones are
	// registered depends on which tests have run.
	got := map[string]bool{}
	for _, n := range strings.Split(resp, ",") {
		got[n] = true
	}
	for _, n := range []string{"first-hit", "last-write-wins", "pool-preferred"} {
		if !got[n] {
			t.Fatalf("merge names: built-in %q missing from %v", n, got)
		}
	}
}
