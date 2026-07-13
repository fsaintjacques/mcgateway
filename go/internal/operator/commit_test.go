package operator

import (
	"errors"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// recordingFS wraps an FS and logs every mutating operation, so tests
// can assert the ordering invariant (modules before config before
// stale deletes) and that no-op commits perform no writes.
type recordingFS struct {
	FS
	ops []string
	// failWrite, when set, makes WriteFile on that path fail.
	failWrite string
}

func (r *recordingFS) WriteFile(name string, data []byte) error {
	if r.failWrite != "" && name == r.failWrite {
		return errors.New("injected write failure")
	}
	r.ops = append(r.ops, "write "+name)
	return r.FS.WriteFile(name, data)
}

func (r *recordingFS) Rename(oldname, newname string) error {
	r.ops = append(r.ops, "rename "+newname)
	return r.FS.Rename(oldname, newname)
}

func (r *recordingFS) Remove(name string) error {
	r.ops = append(r.ops, "remove "+name)
	return r.FS.Remove(name)
}

func (r *recordingFS) opIndex(op string) int {
	return slices.Index(r.ops, op)
}

func tree(config string, modules map[string]string) map[string][]byte {
	out := map[string][]byte{ConfigFile: []byte(config)}
	for name, body := range modules {
		out[UdfDir+"/"+name+".wasm"] = []byte(body)
	}
	return out
}

func mustCommit(t *testing.T, fsys FS, desired map[string][]byte) Result {
	t.Helper()
	res, err := Commit(fsys, desired)
	if err != nil {
		t.Fatalf("commit: %v", err)
	}
	return res
}

func readDisk(t *testing.T, root, rel string) string {
	t.Helper()
	b, err := os.ReadFile(filepath.Join(root, filepath.FromSlash(rel)))
	if err != nil {
		t.Fatalf("read %s: %v", rel, err)
	}
	return string(b)
}

func TestCommitFresh(t *testing.T) {
	root := t.TempDir()
	desired := tree("return {}\n", map[string]string{"m1": "wasm-1", "m2": "wasm-2"})

	res := mustCommit(t, NewOSFS(root), desired)

	if !res.WroteConfig || len(res.WroteModules) != 2 {
		t.Fatalf("unexpected result: %+v", res)
	}
	for rel, want := range desired {
		if got := readDisk(t, root, rel); got != string(want) {
			t.Errorf("%s = %q, want %q", rel, got, want)
		}
	}
}

func TestCommitNoopTouchesNothing(t *testing.T) {
	root := t.TempDir()
	desired := tree("return {}\n", map[string]string{"m1": "wasm-1"})
	mustCommit(t, NewOSFS(root), desired)

	cfgStat, err := os.Stat(filepath.Join(root, ConfigFile))
	if err != nil {
		t.Fatalf("stat config before no-op: %v", err)
	}
	rec := &recordingFS{FS: NewOSFS(root)}
	res := mustCommit(t, rec, desired)

	if len(rec.ops) != 0 {
		t.Errorf("no-op commit performed operations: %v", rec.ops)
	}
	if res.WroteConfig || res.WroteModules != nil || res.RemovedModules != nil {
		t.Errorf("no-op commit reported changes: %+v", res)
	}
	// Belt and braces: the SIGHUP watcher keys off file events, so
	// even metadata must be untouched.
	after, err := os.Stat(filepath.Join(root, ConfigFile))
	if err != nil {
		t.Fatalf("stat config after no-op: %v", err)
	}
	if !after.ModTime().Equal(cfgStat.ModTime()) {
		t.Error("config.lua mtime changed on a no-op commit")
	}
}

func TestCommitModuleLandsBeforeConfig(t *testing.T) {
	root := t.TempDir()
	mustCommit(t, NewOSFS(root), tree("v1\n", nil))

	rec := &recordingFS{FS: NewOSFS(root)}
	mustCommit(t, rec, tree("v2 references m1\n", map[string]string{"m1": "wasm-1"}))

	mod, cfg := rec.opIndex("rename udf/m1.wasm"), rec.opIndex("rename config.lua")
	if mod == -1 || cfg == -1 || mod > cfg {
		t.Fatalf("module must be committed before the config that references it; ops: %v", rec.ops)
	}
}

func TestCommitStaleModuleOutlivesConfig(t *testing.T) {
	root := t.TempDir()
	mustCommit(t, NewOSFS(root), tree("v1 references m1\n", map[string]string{"m1": "wasm-1"}))

	rec := &recordingFS{FS: NewOSFS(root)}
	res := mustCommit(t, rec, tree("v2 references nothing\n", nil))

	cfg, rm := rec.opIndex("rename config.lua"), rec.opIndex("remove udf/m1.wasm")
	if cfg == -1 || rm == -1 || rm < cfg {
		t.Fatalf("stale module must be removed only after the config dropping it is live; ops: %v", rec.ops)
	}
	if len(res.RemovedModules) != 1 {
		t.Fatalf("unexpected result: %+v", res)
	}
}

func TestCommitSweepsCrashLeftovers(t *testing.T) {
	root := t.TempDir()
	desired := tree("return {}\n", map[string]string{"m1": "wasm-1"})
	mustCommit(t, NewOSFS(root), desired)

	// A predecessor died between stage and rename.
	for _, rel := range []string{"config.lua.tmp", "udf/m9.wasm.tmp"} {
		if err := os.WriteFile(filepath.Join(root, filepath.FromSlash(rel)), []byte("junk"), 0o644); err != nil {
			t.Fatal(err)
		}
	}

	rec := &recordingFS{FS: NewOSFS(root)}
	res := mustCommit(t, rec, desired)

	wantSwept := []string{"config.lua.tmp", "udf/m9.wasm.tmp"}
	if !slices.Equal(res.CleanedTemp, wantSwept) {
		t.Fatalf("CleanedTemp = %v, want %v", res.CleanedTemp, wantSwept)
	}
	for _, rel := range wantSwept {
		if _, err := os.Stat(filepath.Join(root, filepath.FromSlash(rel))); !errors.Is(err, os.ErrNotExist) {
			t.Errorf("%s still present after sweep", rel)
		}
	}
	// Sweeping is the only work: the desired tree was already live.
	for _, op := range rec.ops {
		if !strings.HasPrefix(op, "remove ") || !strings.HasSuffix(op, tmpSuffix) {
			t.Errorf("unexpected op alongside sweep: %v", rec.ops)
			break
		}
	}
}

func TestCommitLeavesForeignFilesAlone(t *testing.T) {
	root := t.TempDir()
	mustCommit(t, NewOSFS(root), tree("v1\n", map[string]string{"m1": "wasm-1"}))

	// The wasm host's AOT cache and other non-.wasm entries live in
	// the UDF dir but do not belong to the committer.
	cache := filepath.Join(root, UdfDir, ".cache")
	if err := os.MkdirAll(cache, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(cache, "m1.cwasm"), []byte("aot"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, UdfDir, "notes.txt"), []byte("keep"), 0o644); err != nil {
		t.Fatal(err)
	}
	// Adversarial: directories whose names match the committer's
	// deletion patterns. They must survive — and must not wedge the
	// reconcile loop with unremovable-entry errors either.
	for _, dir := range []string{"udf/decoy.wasm", "udf/stale.tmp"} {
		if err := os.MkdirAll(filepath.Join(root, filepath.FromSlash(dir)), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(root, filepath.FromSlash(dir), "inside"), []byte("x"), 0o644); err != nil {
			t.Fatal(err)
		}
	}

	// Drop m1: only its .wasm may go.
	mustCommit(t, NewOSFS(root), tree("v2\n", nil))

	if got := readDisk(t, root, "udf/.cache/m1.cwasm"); got != "aot" {
		t.Error("AOT cache was touched")
	}
	if got := readDisk(t, root, "udf/notes.txt"); got != "keep" {
		t.Error("foreign file was touched")
	}
	for _, rel := range []string{"udf/decoy.wasm/inside", "udf/stale.tmp/inside"} {
		if got := readDisk(t, root, rel); got != "x" {
			t.Errorf("decoy directory content %s was touched", rel)
		}
	}
	if _, err := os.Stat(filepath.Join(root, UdfDir, "m1.wasm")); !errors.Is(err, os.ErrNotExist) {
		t.Error("stale module survived")
	}
}

func TestCommitRejectsUnrecognizedPaths(t *testing.T) {
	fsys := NewOSFS(t.TempDir())
	for _, rogue := range []string{"rogue.txt", "udf/rogue.txt", "elsewhere/m.wasm"} {
		desired := tree("v1\n", nil)
		desired[rogue] = []byte("x")
		if _, err := Commit(fsys, desired); err == nil {
			t.Errorf("desired tree with %q was not rejected", rogue)
		}
	}
}

func TestCommitModuleFailureLeavesConfigUntouched(t *testing.T) {
	root := t.TempDir()
	mustCommit(t, NewOSFS(root), tree("v1\n", nil))

	rec := &recordingFS{FS: NewOSFS(root), failWrite: "udf/m1.wasm" + tmpSuffix}
	_, err := Commit(rec, tree("v2 references m1\n", map[string]string{"m1": "wasm-1"}))
	if err == nil {
		t.Fatal("expected injected failure to surface")
	}
	if got := readDisk(t, root, ConfigFile); got != "v1\n" {
		t.Fatalf("config advanced past a failed module write: %q", got)
	}
}

func TestCommitRejectsTreeWithoutConfig(t *testing.T) {
	if _, err := Commit(NewOSFS(t.TempDir()), map[string][]byte{"udf/m.wasm": []byte("x")}); err == nil {
		t.Fatal("expected an error for a tree lacking config.lua")
	}
}

func TestCommitConfigOnlyChange(t *testing.T) {
	root := t.TempDir()
	desired := tree("v1\n", map[string]string{"m1": "wasm-1"})
	mustCommit(t, NewOSFS(root), desired)

	rec := &recordingFS{FS: NewOSFS(root)}
	res := mustCommit(t, rec, tree("v2\n", map[string]string{"m1": "wasm-1"}))

	if !res.WroteConfig || res.WroteModules != nil || res.RemovedModules != nil {
		t.Fatalf("unexpected result: %+v", res)
	}
	want := []string{"write config.lua" + tmpSuffix, "rename config.lua"}
	if !slices.Equal(rec.ops, want) {
		t.Fatalf("ops = %v, want %v", rec.ops, want)
	}
}

func TestOSFSRejectsEscapingPaths(t *testing.T) {
	fsys := NewOSFS(t.TempDir())
	for _, name := range []string{"../evil", "/abs", "udf/../../evil"} {
		if err := fsys.WriteFile(name, []byte("x")); err == nil {
			t.Errorf("WriteFile(%q) unexpectedly succeeded", name)
		}
	}
	// Only a whole ".." segment traverses; dots inside a name don't.
	if err := fsys.WriteFile("udf/a..b.wasm", []byte("x")); err != nil {
		t.Errorf("WriteFile(udf/a..b.wasm) rejected: %v", err)
	}
}

// TestCommitRenderIntegration closes the loop with the renderer: a
// rendered snapshot commits cleanly, and re-rendering the same
// snapshot commits as a no-op.
func TestCommitRenderIntegration(t *testing.T) {
	snap := loadCase(t, filepath.Join("testdata", "inline-wasm"))
	files, _ := Render(snap)

	root := t.TempDir()
	res := mustCommit(t, NewOSFS(root), files)
	if !res.WroteConfig {
		t.Fatal("fresh render did not write config")
	}

	rec := &recordingFS{FS: NewOSFS(root)}
	files2, _ := Render(snap)
	if res := mustCommit(t, rec, files2); res.WroteConfig || len(rec.ops) != 0 {
		t.Fatalf("re-render of same snapshot was not a no-op: %+v ops=%v", res, rec.ops)
	}

	if got := readDisk(t, root, "udf/custom_merge.wasm"); got != "fake-wasm-module-one" {
		t.Fatalf("module bytes = %q", got)
	}
}
