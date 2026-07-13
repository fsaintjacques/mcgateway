//go:build kind

package kind_test

import (
	"bytes"
	"context"
	"encoding/binary"
	"fmt"
	"sort"
	"testing"
	"time"

	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	crclient "sigs.k8s.io/controller-runtime/pkg/client"

	v1alpha1 "github.com/fsaintjacques/mcgateway/go/api/v1alpha1"
	mckind "github.com/fsaintjacques/mcgateway/go/internal/kind"
)

// ensureProfileKeyspace applies the `profile` Keyspace CR with the
// prost-based merge module inlined. The module is a build artifact
// the values file cannot carry (it lives baked in the gateway image),
// so the suite creates this CR itself — idempotently, and it stays
// applied: it is part of the release's expected state, like the
// values-seeded CRs.
func ensureProfileKeyspace(t *testing.T, ctx context.Context, s *suite) {
	t.Helper()
	cl := mckind.CRClient(t, s.cfg)
	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)
	ks := &v1alpha1.Keyspace{
		ObjectMeta: metav1.ObjectMeta{Name: "profile", Namespace: s.ns},
		Spec: v1alpha1.KeyspaceSpec{
			Prefix: "profile",
			Read:   []string{"mc-a", "mc-b"},
			Write:  []string{"mc-a", "mc-b"},
			Merge: &v1alpha1.MergeSpec{
				Name: "merge_profile_proto",
				Wasm: bakedModule(t, ctx, s, pod, "merge_profile_proto"),
			},
		},
	}
	if err := cl.Create(ctx, ks); apierrors.IsAlreadyExists(err) {
		// The CR persists across suite runs (created out-of-band, so
		// helm never touches it). Reconcile its spec with the bytes
		// just extracted from the *current* image — otherwise a
		// rebuilt module would silently keep validating the old one.
		var cur v1alpha1.Keyspace
		if err := cl.Get(ctx, crclient.ObjectKeyFromObject(ks), &cur); err != nil {
			t.Fatalf("get existing profile keyspace: %v", err)
		}
		if cur.Spec.Merge == nil || !bytes.Equal(cur.Spec.Merge.Wasm, ks.Spec.Merge.Wasm) {
			cur.Spec = ks.Spec
			if err := cl.Update(ctx, &cur); err != nil {
				t.Fatalf("update profile keyspace with current module: %v", err)
			}
		}
	} else if err != nil {
		t.Fatalf("create profile keyspace: %v", err)
	}
	waitMergeRegistered(t, s, "merge_profile_proto", true)
	// waitPrefixRouted can't be used here: a plain ms/mg round trip
	// writes non-protobuf bytes, which this merge (correctly) treats
	// as undecodable, yielding a miss. Accepting the write is enough
	// to prove the prefix routes.
	deadline := time.Now().Add(reloadWait)
	for {
		if resp, err := mckind.McSet(s.gwAddr, uniqueKey("profile", "probe"), "x"); err == nil && resp.Status == "HD" {
			return
		}
		if time.Now().After(deadline) {
			t.Fatal("profile keyspace never routed")
		}
		time.Sleep(500 * time.Millisecond)
	}
}

// TestWasmProtoMerge is the end-to-end production-shaped loop for the
// WASM UDF path: seed two backends with distinct `Profile` protobufs,
// read through the gateway (which fans out, runs the prost-based
// `merge_profile_proto.wasm` UDF, and frames the synthesized bytes as
// a meta `VA` reply), then decode the returned Profile and assert the
// merge semantics (newest `updated_at` wins for scalar fields;
// string-map attrs unioned with newest-wins on collision).
//
// The module bytes ride an inline-wasm Keyspace CR (see
// ensureProfileKeyspace); the operator lands them in the state dir
// and the gateway's UdfLoader compiles and registers them. A failure
// here means the inline path, the UdfLoader, the VA framing in
// routes.lua, or the merge's prost round-trip broke.
func TestWasmProtoMerge(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()
	ensureProfileKeyspace(t, ctx, s)
	key := uniqueKey("profile", "proto")

	// Two profiles written directly to the backing pools — not via
	// the gateway. This bypasses the write-fan-out path so the test
	// is only about read-side merge, and lets each pool hold
	// distinct bytes.
	a := encodeProfile(profile{
		UserID:    "from-a",
		UpdatedAt: 100,
		Attrs:     map[string]string{"region": "us", "plan": "free"},
	})
	b := encodeProfile(profile{
		UserID:    "from-b",
		UpdatedAt: 500,
		Attrs:     map[string]string{"plan": "pro", "email": "alice@example.com"},
	})

	if _, err := mckind.McSet(s.mcAAddr, key, string(a)); err != nil {
		t.Fatalf("seed mc-a: %v", err)
	}
	if _, err := mckind.McSet(s.mcBAddr, key, string(b)); err != nil {
		t.Fatalf("seed mc-b: %v", err)
	}

	got, err := mckind.McGetWithRetry(s.gwAddr, key, 10)
	if err != nil {
		t.Fatalf("mg via gateway: %v", err)
	}

	merged, err := decodeProfile([]byte(got))
	if err != nil {
		t.Fatalf("decode merged profile: %v (raw=% x)", err, []byte(got))
	}

	// Newest updated_at (500) wins for scalar fields.
	if merged.UserID != "from-b" {
		t.Errorf("user_id: got %q, want %q", merged.UserID, "from-b")
	}
	if merged.UpdatedAt != 500 {
		t.Errorf("updated_at: got %d, want %d", merged.UpdatedAt, 500)
	}

	// Attrs: union with newest-wins on key collision. plan=pro (from
	// b at t=500) replaces plan=free (from a at t=100); region=us
	// keeps since b didn't set it; email=... from b.
	wantAttrs := map[string]string{
		"region": "us",
		"plan":   "pro",
		"email":  "alice@example.com",
	}
	if len(merged.Attrs) != len(wantAttrs) {
		t.Errorf("attrs len: got %d, want %d (%v)", len(merged.Attrs), len(wantAttrs), merged.Attrs)
	}
	for k, v := range wantAttrs {
		if merged.Attrs[k] != v {
			t.Errorf("attrs[%q] = %q, want %q", k, merged.Attrs[k], v)
		}
	}
}

// --- Minimal protobuf wire-format helpers ---
//
// Scoped to the Profile shape the `merge-profile-proto` example
// declares (field 1: string user_id, field 2: int64 updated_at,
// field 3: map<string,string> attrs, field 4: repeated string
// badges). Bringing in `google.golang.org/protobuf` would add a
// codegen step for the kind tree; a ~80-line hand-roll keeps the
// dependency surface tiny.
//
// Wire format reminder:
//   tag = (field_number << 3) | wire_type
//   varint: little-endian base-128, high-bit set = continue
//   wire type 0 (varint) | 2 (length-delim)
//   repeated map<K,V> = repeated embedded message {K at field 1, V at field 2}

type profile struct {
	UserID    string
	UpdatedAt int64
	Attrs     map[string]string
	Badges    []string
}

func encodeProfile(p profile) []byte {
	var out []byte
	if p.UserID != "" {
		out = appendString(out, 1, p.UserID)
	}
	if p.UpdatedAt != 0 {
		out = appendVarintField(out, 2, uint64(p.UpdatedAt))
	}
	// Maps have non-deterministic order in Go; sort for stable
	// encoding. Prost's decoder doesn't care, but sorted bytes are
	// easier to eyeball if something fails.
	keys := make([]string, 0, len(p.Attrs))
	for k := range p.Attrs {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		out = appendMapEntry(out, 3, k, p.Attrs[k])
	}
	for _, b := range p.Badges {
		out = appendString(out, 4, b)
	}
	return out
}

func appendVarint(out []byte, v uint64) []byte {
	var buf [binary.MaxVarintLen64]byte
	n := binary.PutUvarint(buf[:], v)
	return append(out, buf[:n]...)
}

func appendVarintField(out []byte, field int, v uint64) []byte {
	out = appendVarint(out, uint64(field)<<3)
	return appendVarint(out, v)
}

func appendString(out []byte, field int, s string) []byte {
	out = appendVarint(out, (uint64(field)<<3)|2)
	out = appendVarint(out, uint64(len(s)))
	return append(out, s...)
}

// appendMapEntry wraps {K,V} in an embedded-message entry under the
// map field number. K is tag 1 (string), V is tag 2 (string).
func appendMapEntry(out []byte, field int, k, v string) []byte {
	var entry []byte
	entry = appendString(entry, 1, k)
	entry = appendString(entry, 2, v)
	out = appendVarint(out, (uint64(field)<<3)|2)
	out = appendVarint(out, uint64(len(entry)))
	return append(out, entry...)
}

func decodeProfile(b []byte) (profile, error) {
	var p profile
	p.Attrs = make(map[string]string)
	for len(b) > 0 {
		tag, n := binary.Uvarint(b)
		if n <= 0 {
			return p, fmt.Errorf("bad tag varint")
		}
		b = b[n:]
		field := int(tag >> 3)
		wire := int(tag & 0x7)
		switch {
		case field == 1 && wire == 2: // user_id
			s, rest, err := readString(b)
			if err != nil {
				return p, fmt.Errorf("user_id: %w", err)
			}
			p.UserID = s
			b = rest
		case field == 2 && wire == 0: // updated_at
			v, n := binary.Uvarint(b)
			if n <= 0 {
				return p, fmt.Errorf("updated_at varint")
			}
			p.UpdatedAt = int64(v)
			b = b[n:]
		case field == 3 && wire == 2: // attrs entry
			entry, rest, err := readBytes(b)
			if err != nil {
				return p, fmt.Errorf("attrs: %w", err)
			}
			k, v, err := decodeMapEntry(entry)
			if err != nil {
				return p, fmt.Errorf("attrs entry: %w", err)
			}
			p.Attrs[k] = v
			b = rest
		case field == 4 && wire == 2: // badges
			s, rest, err := readString(b)
			if err != nil {
				return p, fmt.Errorf("badges: %w", err)
			}
			p.Badges = append(p.Badges, s)
			b = rest
		default:
			return p, fmt.Errorf("unknown (field=%d, wire=%d)", field, wire)
		}
	}
	return p, nil
}

func readString(b []byte) (string, []byte, error) {
	bs, rest, err := readBytes(b)
	return string(bs), rest, err
}

func readBytes(b []byte) ([]byte, []byte, error) {
	ln, n := binary.Uvarint(b)
	if n <= 0 {
		return nil, nil, fmt.Errorf("length varint")
	}
	b = b[n:]
	if uint64(len(b)) < ln {
		return nil, nil, fmt.Errorf("truncated: need %d, have %d", ln, len(b))
	}
	return b[:ln], b[ln:], nil
}

func decodeMapEntry(b []byte) (string, string, error) {
	var k, v string
	for len(b) > 0 {
		tag, n := binary.Uvarint(b)
		if n <= 0 {
			return "", "", fmt.Errorf("entry tag")
		}
		b = b[n:]
		field := int(tag >> 3)
		wire := int(tag & 0x7)
		if wire != 2 {
			return "", "", fmt.Errorf("entry wire=%d", wire)
		}
		s, rest, err := readString(b)
		if err != nil {
			return "", "", err
		}
		switch field {
		case 1:
			k = s
		case 2:
			v = s
		default:
			// Unknown map-entry field; skip.
		}
		b = rest
	}
	return k, v, nil
}
