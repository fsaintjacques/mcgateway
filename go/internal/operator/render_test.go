package operator

import (
	"bytes"
	"flag"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	sigsyaml "sigs.k8s.io/yaml"

	v1alpha1 "github.com/fsaintjacques/mcgateway/go/api/v1alpha1"
)

var update = flag.Bool("update", false, "rewrite golden files from current Render output")

// snapshotYAML is the on-disk shape of a golden case's input.
type snapshotYAML struct {
	Pools     []v1alpha1.Pool     `json:"pools"`
	Keyspaces []v1alpha1.Keyspace `json:"keyspaces"`
}

func loadCase(t *testing.T, dir string) Snapshot {
	t.Helper()
	raw, err := os.ReadFile(filepath.Join(dir, "input.yaml"))
	if err != nil {
		t.Fatalf("read input: %v", err)
	}
	var in snapshotYAML
	if err := sigsyaml.UnmarshalStrict(raw, &in); err != nil {
		t.Fatalf("parse input: %v", err)
	}
	return Snapshot{Pools: in.Pools, Keyspaces: in.Keyspaces}
}

func warningsText(warns []Warning) []byte {
	if len(warns) == 0 {
		return nil
	}
	var b strings.Builder
	for _, w := range warns {
		b.WriteString(w.String())
		b.WriteByte('\n')
	}
	return []byte(b.String())
}

// goldenFiles reads every expected output file in the case dir:
// config.lua, udf/*.wasm, and warnings.txt (absent means no warnings).
func goldenFiles(t *testing.T, dir string) map[string][]byte {
	t.Helper()
	out := map[string][]byte{}
	err := filepath.WalkDir(dir, func(path string, d os.DirEntry, err error) error {
		if err != nil || d.IsDir() {
			return err
		}
		rel, _ := filepath.Rel(dir, path)
		rel = filepath.ToSlash(rel)
		if rel == "input.yaml" {
			return nil
		}
		b, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		out[rel] = b
		return nil
	})
	if err != nil {
		t.Fatalf("walk goldens: %v", err)
	}
	return out
}

func TestRenderGoldens(t *testing.T) {
	cases, err := os.ReadDir("testdata")
	if err != nil {
		t.Fatalf("read testdata: %v", err)
	}
	for _, c := range cases {
		if !c.IsDir() {
			continue
		}
		t.Run(c.Name(), func(t *testing.T) {
			dir := filepath.Join("testdata", c.Name())
			files, warns := Render(loadCase(t, dir))

			got := map[string][]byte{}
			for k, v := range files {
				got[k] = v
			}
			if w := warningsText(warns); w != nil {
				got["warnings.txt"] = w
			}

			if *update {
				// Rewrite the case dir from scratch (input.yaml excepted).
				for k := range goldenFiles(t, dir) {
					if err := os.Remove(filepath.Join(dir, filepath.FromSlash(k))); err != nil {
						t.Fatalf("clean golden %s: %v", k, err)
					}
				}
				for k, v := range got {
					path := filepath.Join(dir, filepath.FromSlash(k))
					if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
						t.Fatalf("mkdir for %s: %v", k, err)
					}
					if err := os.WriteFile(path, v, 0o644); err != nil {
						t.Fatalf("write golden %s: %v", k, err)
					}
				}
				return
			}

			want := goldenFiles(t, dir)
			for k, wv := range want {
				gv, ok := got[k]
				if !ok {
					t.Errorf("missing output %s", k)
					continue
				}
				if !bytes.Equal(gv, wv) {
					t.Errorf("output %s differs from golden:\n--- got ---\n%s\n--- want ---\n%s", k, gv, wv)
				}
			}
			for k := range got {
				if _, ok := want[k]; !ok {
					t.Errorf("unexpected output %s:\n%s", k, got[k])
				}
			}
		})
	}
}

// TestRenderDeterministic renders every golden case twice — once with
// the input order reversed — and requires byte-identical output. The
// committer diffs rendered bytes against disk, so any order dependence
// would masquerade as config churn (and spurious SIGHUPs).
func TestRenderDeterministic(t *testing.T) {
	cases, err := os.ReadDir("testdata")
	if err != nil {
		t.Fatalf("read testdata: %v", err)
	}
	for _, c := range cases {
		if !c.IsDir() {
			continue
		}
		t.Run(c.Name(), func(t *testing.T) {
			snap := loadCase(t, filepath.Join("testdata", c.Name()))
			first, firstWarns := Render(snap)

			slices.Reverse(snap.Pools)
			slices.Reverse(snap.Keyspaces)
			second, secondWarns := Render(snap)

			if len(first) != len(second) {
				t.Fatalf("file sets differ: %d vs %d", len(first), len(second))
			}
			for k, v := range first {
				if !bytes.Equal(v, second[k]) {
					t.Errorf("output %s depends on input order:\n--- original ---\n%s\n--- reversed ---\n%s", k, v, second[k])
				}
			}
			// Warnings are golden-compared and operator-logged; their
			// order must be as order-independent as the files.
			if got, want := warningsText(secondWarns), warningsText(firstWarns); !bytes.Equal(got, want) {
				t.Errorf("warnings depend on input order:\n--- original ---\n%s\n--- reversed ---\n%s", want, got)
			}
		})
	}
}

// TestLuaQuote pins the escaping rules the renderer relies on for
// defense against non-apiserver callers.
func TestLuaQuote(t *testing.T) {
	for in, want := range map[string]string{
		"plain":      `"plain"`,
		`q"uote`:     `"q\"uote"`,
		`back\slash`: `"back\\slash"`,
		"new\nline":  `"new\010line"`,
		"nul\x00":    `"nul\000"`,
	} {
		if got := luaQuote(in); got != want {
			t.Errorf("luaQuote(%q) = %s, want %s", in, got, want)
		}
	}
}

// TestRenderEmptySnapshotIsValidConfig documents that an empty
// snapshot still renders a loadable (empty) config — a gateway with
// no CRs yet must boot, not crash-loop on a missing file.
func TestRenderEmptySnapshotIsValidConfig(t *testing.T) {
	files, warns := Render(Snapshot{})
	if len(warns) != 0 {
		t.Fatalf("unexpected warnings: %v", warns)
	}
	cfg, ok := files[ConfigFile]
	if !ok {
		t.Fatal("no config.lua rendered")
	}
	for _, want := range []string{"pools = {", "keyspaces = {"} {
		if !strings.Contains(string(cfg), want) {
			t.Errorf("config.lua missing %q:\n%s", want, cfg)
		}
	}
	if len(files) != 1 {
		t.Errorf("expected only config.lua, got %d files", len(files))
	}
}
