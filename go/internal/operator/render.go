// Package operator compiles Pool/Keyspace CR snapshots into the file
// tree the gateway consumes: a config.lua data literal plus one .wasm
// file per inline merge module. Rendering is a pure function of the
// snapshot — deterministic to the byte, no clock, no filesystem — so
// the committer can diff its output against disk and golden tests can
// pin it exactly.
package operator

import (
	"bytes"
	"fmt"
	"sort"
	"strings"

	v1alpha1 "github.com/fsaintjacques/mcgateway/go/api/v1alpha1"
)

// ConfigFile is the rendered config's path, relative to the mount root.
const ConfigFile = "config.lua"

// UdfDir is the rendered modules' directory, relative to the mount root.
const UdfDir = "udf"

// builtinMerges are the merge names compiled into libmcgateway. A
// keyspace's merge must resolve to one of these or to an inline module
// in the same snapshot — the gateway's validator rejects a whole
// config over one unknown name, so the renderer must never emit one.
// The cross-language contract test pins this list against the Lua test
// harness's registry.
var builtinMerges = map[string]bool{
	"first-hit":       true,
	"last-write-wins": true,
	"pool-preferred":  true,
}

var validHash = map[string]bool{"xxhash": true, "md5": true, "crc32": true}
var validDist = map[string]bool{"ring_hash": true, "jump_hash": true}
var validWritePolicy = map[string]bool{"all": true, "first": true}

// Snapshot is the full set of CRs in the namespace at render time.
type Snapshot struct {
	Pools     []v1alpha1.Pool
	Keyspaces []v1alpha1.Keyspace
}

// Warning records a CR that was skipped (or partially honoured)
// during rendering. One bad CR degrades to a Warning, never to a
// failed render: the emitted file must always pass the gateway's
// validator, and every healthy CR must keep serving.
type Warning struct {
	Kind    string // "Pool" or "Keyspace"
	Name    string // metadata.name
	Message string
}

func (w Warning) String() string {
	return fmt.Sprintf("%s %q: %s", w.Kind, w.Name, w.Message)
}

// Render compiles the snapshot into the desired file tree: path →
// content, with paths relative to the mount root ("config.lua",
// "udf/<name>.wasm"). The same snapshot renders the same bytes,
// regardless of input order.
func Render(s Snapshot) (map[string][]byte, []Warning) {
	var warns []Warning

	pools := renderablePools(dedupByName("Pool", s.Pools, func(p v1alpha1.Pool) string { return p.Name }, &warns), &warns)
	poolNames := map[string]bool{}
	for _, p := range pools {
		poolNames[p.Name] = true
	}

	keyspaces := dedupByName("Keyspace", s.Keyspaces, func(k v1alpha1.Keyspace) string { return k.Name }, &warns)
	modules := collectModules(keyspaces, &warns)
	rendered := renderableKeyspaces(keyspaces, poolNames, modules, &warns)

	files := map[string][]byte{
		ConfigFile: renderConfig(pools, rendered),
	}
	for name, wasm := range modules {
		files[UdfDir+"/"+name+".wasm"] = wasm
	}
	return files, warns
}

// dedupByName drops every CR whose metadata.name is shared with
// another CR of the same kind, one warning per contested name. The
// apiserver makes this impossible; for any other caller a duplicate
// name is an ambiguous identity, and a defensive re-check refuses to
// guess a winner — sorting cannot break the tie deterministically
// when the specs differ. The survivors have unique names, which is
// what makes the by-name sorts downstream order-independent.
func dedupByName[T any](kind string, in []T, name func(T) string, warns *[]Warning) []T {
	count := map[string]int{}
	for _, it := range in {
		count[name(it)]++
	}
	var contested []string
	for n, c := range count {
		if c > 1 {
			contested = append(contested, n)
		}
	}
	sort.Strings(contested)
	for _, n := range contested {
		*warns = append(*warns, Warning{kind, n, fmt.Sprintf("skipped: %d %s objects share this name", count[n], kind)})
	}

	var out []T
	for _, it := range in {
		if count[name(it)] == 1 {
			out = append(out, it)
		}
	}
	return out
}

// renderablePools validates pools and returns them sorted by name.
// The API server enforces most of this (schema validation, name
// uniqueness); the renderer re-checks so its guarantee — emitted
// config always passes the Lua validator — holds for any caller, not
// just ones fronted by an apiserver.
func renderablePools(in []v1alpha1.Pool, warns *[]Warning) []v1alpha1.Pool {
	sorted := append([]v1alpha1.Pool(nil), in...)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i].Name < sorted[j].Name })

	var out []v1alpha1.Pool
	for _, p := range sorted {
		warn := func(msg string) { *warns = append(*warns, Warning{"Pool", p.Name, msg}) }
		if p.Name == "" {
			warn("skipped: empty name")
			continue
		}
		if len(p.Spec.Addrs) == 0 {
			warn("skipped: addrs must be a non-empty list")
			continue
		}
		if bad := firstBad(p.Spec.Addrs); bad != -1 {
			warn(fmt.Sprintf("skipped: addrs[%d] is empty", bad))
			continue
		}
		if p.Spec.Hash != "" && !validHash[p.Spec.Hash] {
			warn(fmt.Sprintf("skipped: invalid hash %q", p.Spec.Hash))
			continue
		}
		if p.Spec.Dist != "" && !validDist[p.Spec.Dist] {
			warn(fmt.Sprintf("skipped: invalid dist %q", p.Spec.Dist))
			continue
		}
		out = append(out, p)
	}
	return out
}

// collectModules gathers inline wasm modules from the snapshot's
// keyspaces: merge name → module bytes. Conflict rules match the
// gateway loader's semantics: a name colliding with a built-in is
// dropped (the built-in would shadow it anyway), two keyspaces
// inlining different bytes under one name resolve to the
// lexicographically-first keyspace's bytes, identical bytes dedupe
// silently. Keyspaces are processed in metadata.name order so the
// winner is deterministic.
//
// Collection deliberately runs before keyspace validation: a keyspace
// skipped for its own defects (bad prefix, unknown pool) still
// publishes its inline module, and a module referenced by no rendered
// keyspace is still emitted. Modules are pod-global capabilities;
// config is the routing surface ("disk is the capability surface").
// Registering an unreferenced module is a no-op by design, and this
// keeps a routing-level mistake in one CR from yanking a module other
// keyspaces resolve against.
func collectModules(in []v1alpha1.Keyspace, warns *[]Warning) map[string][]byte {
	sorted := append([]v1alpha1.Keyspace(nil), in...)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i].Name < sorted[j].Name })

	modules := map[string][]byte{}
	for _, ks := range sorted {
		m := ks.Spec.Merge
		if m == nil || len(m.Wasm) == 0 {
			continue
		}
		warn := func(msg string) { *warns = append(*warns, Warning{"Keyspace", ks.Name, msg}) }
		if !safeModuleName(m.Name) {
			warn(fmt.Sprintf("inline module %q dropped: invalid name", m.Name))
			continue
		}
		if builtinMerges[m.Name] {
			warn(fmt.Sprintf("inline module %q dropped: name collides with a built-in merge (built-in wins)", m.Name))
			continue
		}
		if prev, ok := modules[m.Name]; ok {
			if !bytes.Equal(prev, m.Wasm) {
				warn(fmt.Sprintf("inline module %q ignored: another Keyspace already inlines different bytes under this name (first by name wins)", m.Name))
			}
			continue
		}
		modules[m.Name] = m.Wasm
	}
	return modules
}

// renderableKeyspaces validates keyspaces against the Lua validator's
// rules and returns them sorted by prefix. Conflict resolution (the
// duplicate-prefix rule) processes keyspaces in metadata.name order so
// the winner is deterministic.
func renderableKeyspaces(in []v1alpha1.Keyspace, poolNames map[string]bool, modules map[string][]byte, warns *[]Warning) []v1alpha1.Keyspace {
	sorted := append([]v1alpha1.Keyspace(nil), in...)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i].Name < sorted[j].Name })

	var out []v1alpha1.Keyspace
	seenPrefix := map[string]bool{}
	for _, ks := range sorted {
		warn := func(msg string) { *warns = append(*warns, Warning{"Keyspace", ks.Name, msg}) }
		spec := ks.Spec
		if spec.Prefix == "" {
			warn("skipped: empty prefix")
			continue
		}
		if strings.Contains(spec.Prefix, ":") {
			warn(fmt.Sprintf("skipped: prefix %q must not contain ':'", spec.Prefix))
			continue
		}
		if spec.Prefix == "__udf" || spec.Prefix == "__mcgw" {
			warn(fmt.Sprintf("skipped: prefix %q is reserved", spec.Prefix))
			continue
		}
		if seenPrefix[spec.Prefix] {
			warn(fmt.Sprintf("skipped: duplicate prefix %q (first Keyspace by name wins)", spec.Prefix))
			continue
		}
		if msg := checkPoolList("read", spec.Read, poolNames); msg != "" {
			warn("skipped: " + msg)
			continue
		}
		if msg := checkPoolList("write", spec.Write, poolNames); msg != "" {
			warn("skipped: " + msg)
			continue
		}
		if spec.WritePolicy != "" && !validWritePolicy[spec.WritePolicy] {
			warn(fmt.Sprintf("skipped: invalid writePolicy %q", spec.WritePolicy))
			continue
		}
		if m := spec.Merge; m != nil {
			_, inline := modules[m.Name]
			if !builtinMerges[m.Name] && !inline {
				warn(fmt.Sprintf("skipped: merge %q is neither a built-in nor an inline module in this snapshot", m.Name))
				continue
			}
		}
		seenPrefix[spec.Prefix] = true
		out = append(out, ks)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Spec.Prefix < out[j].Spec.Prefix })
	return out
}

func checkPoolList(field string, list []string, poolNames map[string]bool) string {
	if len(list) == 0 {
		return field + " must be a non-empty list of pool names"
	}
	seen := map[string]bool{}
	for i, name := range list {
		if name == "" {
			return fmt.Sprintf("%s[%d] is empty", field, i)
		}
		if !poolNames[name] {
			return fmt.Sprintf("%s[%d] references unknown pool %q", field, i, name)
		}
		if seen[name] {
			return fmt.Sprintf("%s lists pool %q twice", field, name)
		}
		seen[name] = true
	}
	return ""
}

// safeModuleName bounds what the renderer will use as a file stem.
// The CRD pattern already enforces this; re-checking keeps path
// construction safe for any caller.
func safeModuleName(name string) bool {
	if name == "" {
		return false
	}
	for i := 0; i < len(name); i++ {
		c := name[i]
		alnum := c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9'
		if !alnum && c != '_' && c != '-' {
			return false
		}
	}
	return name[0] != '_' && name[0] != '-'
}

// renderConfig emits the Lua data literal. Field order is fixed and
// entries are pre-sorted, so identical snapshots produce identical
// bytes — no timestamps, no provenance comments.
func renderConfig(pools []v1alpha1.Pool, keyspaces []v1alpha1.Keyspace) []byte {
	var b strings.Builder
	b.WriteString("return {\n")

	b.WriteString("    pools = {\n")
	for _, p := range pools {
		fmt.Fprintf(&b, "        { name = %s, addrs = { %s }", luaQuote(p.Name), luaQuoteList(p.Spec.Addrs))
		if p.Spec.Hash != "" {
			fmt.Fprintf(&b, ", hash = %s", luaQuote(p.Spec.Hash))
		}
		if p.Spec.Dist != "" {
			fmt.Fprintf(&b, ", dist = %s", luaQuote(p.Spec.Dist))
		}
		b.WriteString(" },\n")
	}
	b.WriteString("    },\n")

	b.WriteString("    keyspaces = {\n")
	for _, ks := range keyspaces {
		spec := ks.Spec
		fmt.Fprintf(&b, "        { prefix = %s,\n", luaQuote(spec.Prefix))
		fmt.Fprintf(&b, "          read = { %s },\n", luaQuoteList(spec.Read))
		fmt.Fprintf(&b, "          write = { %s }", luaQuoteList(spec.Write))
		if spec.WritePolicy != "" {
			fmt.Fprintf(&b, ",\n          write_policy = %s", luaQuote(spec.WritePolicy))
		}
		if spec.Merge != nil {
			fmt.Fprintf(&b, ",\n          merge = %s", luaQuote(spec.Merge.Name))
		}
		b.WriteString(" },\n")
	}
	b.WriteString("    },\n")

	b.WriteString("}\n")
	return []byte(b.String())
}

func luaQuoteList(items []string) string {
	quoted := make([]string, len(items))
	for i, s := range items {
		quoted[i] = luaQuote(s)
	}
	return strings.Join(quoted, ", ")
}

// luaQuote renders s as a Lua string literal. Printable ASCII passes
// through; everything else uses Lua's decimal escape, which every Lua
// 5.x understands (Go's %q would emit \uXXXX forms Lua cannot parse).
// Rendered values are schema-constrained ASCII already; escaping is
// defense for non-apiserver callers.
func luaQuote(s string) string {
	var b strings.Builder
	b.WriteByte('"')
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case c == '"':
			b.WriteString(`\"`)
		case c == '\\':
			b.WriteString(`\\`)
		case c >= 0x20 && c < 0x7f:
			b.WriteByte(c)
		default:
			fmt.Fprintf(&b, "\\%03d", c)
		}
	}
	b.WriteByte('"')
	return b.String()
}

func firstBad(items []string) int {
	for i, s := range items {
		if s == "" {
			return i
		}
	}
	return -1
}
