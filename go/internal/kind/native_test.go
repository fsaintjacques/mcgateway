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

	got := strings.Split(resp, ",")
	want := map[string]bool{
		"first-hit":       true,
		"last-write-wins": true,
		"pool-preferred":  true,
	}
	if len(got) != len(want) {
		t.Fatalf("merge names: got %v, want all of %v", got, want)
	}
	for _, n := range got {
		if !want[n] {
			t.Fatalf("unexpected merge name %q in %v", n, got)
		}
	}
}
