package operator

import (
	"bytes"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// FS is the committer's view of the mount root. Paths are
// slash-separated and relative to the root ("config.lua",
// "udf/x.wasm"). The one non-obvious contract: WriteFile creates
// missing parent directories, and both WriteFile and Rename are
// durable (fsynced) before returning — the gateway's watchers react
// to renames, so a rename must never become visible before its
// content is on disk. Implementations exist for the local mount
// (NewOSFS) and tests; Stage 5's GCS mount slots in here.
type FS interface {
	// ReadFile returns fs.ErrNotExist-wrapped errors for absent files.
	ReadFile(name string) ([]byte, error)
	WriteFile(name string, data []byte) error
	// Rename atomically replaces newname with oldname.
	Rename(oldname, newname string) error
	Remove(name string) error
	// ReadDir lists the names of regular files in name; directories
	// and other non-file entries are omitted, so nothing the
	// committer sweeps or deletes can be a directory (a foreign
	// directory named *.wasm must neither be removed nor wedge the
	// reconcile loop with unremovable-entry errors). An absent
	// directory is an empty listing, not an error.
	ReadDir(name string) ([]string, error)
}

// tmpSuffix marks in-flight writes. Both gateway watchers ignore it:
// the config watcher matches the exact config file name, and the UDF
// loader only reacts to the .wasm extension. A leftover temp file is
// therefore inert until the sweep in the next commit removes it.
const tmpSuffix = ".tmp"

// Result reports what a commit changed, mostly for operator logs.
// WroteConfig true means the gateway is about to receive (at most)
// one SIGHUP for this commit.
type Result struct {
	WroteConfig    bool
	WroteModules   []string
	RemovedModules []string
	CleanedTemp    []string
}

// Commit diffs the rendered tree against the mount and applies the
// difference in an order that keeps every intermediate state loadable
// by the gateway:
//
//  1. sweep temp leftovers from a crashed predecessor (inert, see
//     tmpSuffix, but they must not accumulate),
//  2. write new/changed modules (temp-then-rename) — registering a
//     module the live config doesn't reference yet is a no-op,
//  3. write config.lua iff its bytes changed — the only step that
//     triggers a reload,
//  4. remove stale modules — only now, after the config that no
//     longer references them is live; in-flight merges hold the old
//     registry snapshot and complete safely.
//
// A commit that changes nothing performs no writes at all, which is
// what keeps "one SIGHUP per config change" honest. Errors abort the
// commit where it stands; the invariant above makes every partial
// state safe, and the reconciler's next pass converges.
func Commit(fsys FS, desired map[string][]byte) (Result, error) {
	var res Result

	// The renderer always emits a config; a tree without one is a
	// caller bug, and "committing" it would replace the gateway's
	// config with nothing.
	if _, ok := desired[ConfigFile]; !ok {
		return res, fmt.Errorf("desired tree lacks %s", ConfigFile)
	}
	// Refuse shapes this committer does not know how to place or
	// clean up. Silently ignoring one would let the config go live
	// referencing a file that was never written.
	for _, path := range sortedKeys(desired) {
		if path == ConfigFile {
			continue
		}
		if !strings.HasPrefix(path, UdfDir+"/") || !strings.HasSuffix(path, ".wasm") {
			return res, fmt.Errorf("desired tree contains unrecognized path %q", path)
		}
	}

	if err := sweepTemp(fsys, &res); err != nil {
		return res, err
	}

	// New or changed modules first.
	for _, path := range sortedKeys(desired) {
		if !strings.HasPrefix(path, UdfDir+"/") {
			continue
		}
		changed, err := writeIfChanged(fsys, path, desired[path])
		if err != nil {
			return res, err
		}
		if changed {
			res.WroteModules = append(res.WroteModules, path)
		}
	}

	// Config next; the gateway watcher fires on this rename.
	changed, err := writeIfChanged(fsys, ConfigFile, desired[ConfigFile])
	if err != nil {
		return res, err
	}
	res.WroteConfig = changed

	// Stale modules last, once nothing routes through them. Only
	// *.wasm files are ours to delete: the wasm host keeps its AOT
	// cache under the UDF dir (.cache/), and whatever else lives
	// there is not the committer's business.
	names, err := fsys.ReadDir(UdfDir)
	if err != nil {
		return res, fmt.Errorf("list %s: %w", UdfDir, err)
	}
	sort.Strings(names)
	for _, name := range names {
		if !strings.HasSuffix(name, ".wasm") {
			continue
		}
		path := UdfDir + "/" + name
		if _, ok := desired[path]; ok {
			continue
		}
		if err := fsys.Remove(path); err != nil {
			return res, fmt.Errorf("remove stale %s: %w", path, err)
		}
		res.RemovedModules = append(res.RemovedModules, path)
	}

	return res, nil
}

// writeIfChanged commits data to path via temp-then-rename, skipping
// the write entirely when the on-disk bytes already match.
func writeIfChanged(fsys FS, path string, data []byte) (bool, error) {
	cur, err := fsys.ReadFile(path)
	switch {
	case err == nil:
		if bytes.Equal(cur, data) {
			return false, nil
		}
	case errors.Is(err, fs.ErrNotExist):
		// First write.
	default:
		return false, fmt.Errorf("read %s: %w", path, err)
	}

	tmp := path + tmpSuffix
	if err := fsys.WriteFile(tmp, data); err != nil {
		return false, fmt.Errorf("stage %s: %w", tmp, err)
	}
	if err := fsys.Rename(tmp, path); err != nil {
		return false, fmt.Errorf("commit %s: %w", path, err)
	}
	return true, nil
}

// sweepTemp removes in-flight files a crashed predecessor left in the
// two directories the committer stages into.
func sweepTemp(fsys FS, res *Result) error {
	for _, dir := range []string{"", UdfDir} {
		names, err := fsys.ReadDir(dir)
		if err != nil {
			return fmt.Errorf("list %q: %w", dir, err)
		}
		sort.Strings(names)
		for _, name := range names {
			if !strings.HasSuffix(name, tmpSuffix) {
				continue
			}
			path := name
			if dir != "" {
				path = dir + "/" + name
			}
			if err := fsys.Remove(path); err != nil {
				return fmt.Errorf("sweep %s: %w", path, err)
			}
			res.CleanedTemp = append(res.CleanedTemp, path)
		}
	}
	return nil
}

func sortedKeys(m map[string][]byte) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

// osFS implements FS on a local mount root with the durability the
// interface demands: files are fsynced before close, and renames
// fsync the parent directory so the new directory entry survives a
// crash. On the emptyDir topology this is cheap insurance; on Stage
// 5's remote mounts the equivalent guarantees move into that FS.
type osFS struct {
	root string
}

// NewOSFS returns an FS rooted at dir.
func NewOSFS(dir string) FS { return osFS{root: dir} }

func (o osFS) abs(name string) (string, error) {
	if name == "" {
		return o.root, nil
	}
	if filepath.IsAbs(name) {
		return "", fmt.Errorf("path %q escapes the mount root", name)
	}
	// Per-segment check: only a segment that IS ".." traverses;
	// names merely containing dots ("a..b.wasm") are legitimate.
	for _, seg := range strings.Split(name, "/") {
		if seg == ".." {
			return "", fmt.Errorf("path %q escapes the mount root", name)
		}
	}
	return filepath.Join(o.root, filepath.FromSlash(name)), nil
}

func (o osFS) ReadFile(name string) ([]byte, error) {
	path, err := o.abs(name)
	if err != nil {
		return nil, err
	}
	return os.ReadFile(path)
}

func (o osFS) WriteFile(name string, data []byte) error {
	path, err := o.abs(name)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o644)
	if err != nil {
		return err
	}
	if _, err := f.Write(data); err != nil {
		f.Close()
		return err
	}
	if err := f.Sync(); err != nil {
		f.Close()
		return err
	}
	return f.Close()
}

func (o osFS) Rename(oldname, newname string) error {
	oldpath, err := o.abs(oldname)
	if err != nil {
		return err
	}
	newpath, err := o.abs(newname)
	if err != nil {
		return err
	}
	if err := os.Rename(oldpath, newpath); err != nil {
		return err
	}
	return syncDir(filepath.Dir(newpath))
}

func (o osFS) Remove(name string) error {
	path, err := o.abs(name)
	if err != nil {
		return err
	}
	return os.Remove(path)
}

func (o osFS) ReadDir(name string) ([]string, error) {
	path, err := o.abs(name)
	if err != nil {
		return nil, err
	}
	entries, err := os.ReadDir(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var names []string
	for _, e := range entries {
		if e.Type().IsRegular() {
			names = append(names, e.Name())
		}
	}
	return names, nil
}

func syncDir(dir string) error {
	d, err := os.Open(dir)
	if err != nil {
		return err
	}
	err = d.Sync()
	if cerr := d.Close(); err == nil {
		err = cerr
	}
	return err
}
