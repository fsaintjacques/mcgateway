package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// ---------------------------------------------------------------------------
// Pool CRD
// ---------------------------------------------------------------------------

// +kubebuilder:object:root=true
// +kubebuilder:printcolumn:name="Addrs",type=string,JSONPath=`.spec.addrs`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// Pool defines a named backend: a set of addresses plus the
// client-side hashing used to spread keys across them. The pool's
// name is metadata.name — a Pool is a reference to backends that
// already exist; the operator never provisions or scales them.
type Pool struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`
	Spec              PoolSpec `json:"spec"`
}

// +kubebuilder:object:root=true

// PoolList contains a list of Pool.
type PoolList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []Pool `json:"items"`
}

// PoolSpec defines the backend addresses and key-distribution
// behaviour of a pool.
type PoolSpec struct {
	// Addrs lists backend addresses as "host:port". Order is
	// significant for distribution stability; a duplicated address
	// would silently skew key distribution, hence the uniqueness
	// rule. Item length is bounded to a DNS-1123 name plus port.
	// +kubebuilder:validation:MinItems=1
	// +kubebuilder:validation:MaxItems=64
	// +kubebuilder:validation:items:MinLength=1
	// +kubebuilder:validation:items:MaxLength=259
	// +kubebuilder:validation:XValidation:rule="self.all(x, self.filter(y, y == x).size() == 1)",message="addrs must be unique"
	// +listType=atomic
	Addrs []string `json:"addrs"`

	// Hash selects the key-hashing function. The gateway default
	// applies when unset.
	// +optional
	// +kubebuilder:validation:Enum=xxhash;md5;crc32
	Hash string `json:"hash,omitempty"`

	// Dist selects the key-distribution strategy across Addrs.
	// +optional
	// +kubebuilder:validation:Enum=ring_hash;jump_hash
	Dist string `json:"dist,omitempty"`
}

// ---------------------------------------------------------------------------
// Keyspace CRD
// ---------------------------------------------------------------------------

// +kubebuilder:object:root=true
// +kubebuilder:printcolumn:name="Prefix",type=string,JSONPath=`.spec.prefix`
// +kubebuilder:printcolumn:name="Merge",type=string,JSONPath=`.spec.merge.name`
// +kubebuilder:printcolumn:name="WritePolicy",type=string,JSONPath=`.spec.writePolicy`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// Keyspace binds a key prefix to backend pools with a merge strategy
// and a write policy. The routing key is spec.prefix; metadata.name
// is only the object's identity.
type Keyspace struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`
	Spec              KeyspaceSpec `json:"spec"`
}

// +kubebuilder:object:root=true

// KeyspaceList contains a list of Keyspace.
type KeyspaceList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []Keyspace `json:"items"`
}

// KeyspaceSpec defines the routing, merge, and write behaviour for a
// key prefix.
type KeyspaceSpec struct {
	// Prefix routes keys of the form "<prefix>:<rest>". Must be
	// unique across Keyspaces in the namespace (renderer-enforced).
	// The pattern is deliberately stricter than the Lua validator
	// (which bans only ':' and the exact reserved names): no leading
	// underscore reserves the whole "__" control namespace, and a
	// bounded charset keeps prefixes log- and metric-label-safe.
	// +kubebuilder:validation:MinLength=1
	// +kubebuilder:validation:MaxLength=128
	// +kubebuilder:validation:Pattern=`^[A-Za-z0-9][A-Za-z0-9_.-]*$`
	Prefix string `json:"prefix"`

	// Read lists pools fanned out to on reads, in preference order
	// (pool-preferred consumes this order). listType=atomic rather
	// than set: order is load-bearing and server-side-apply merge
	// semantics on sets do not preserve it.
	// +kubebuilder:validation:MinItems=1
	// +kubebuilder:validation:MaxItems=16
	// +kubebuilder:validation:items:MinLength=1
	// +kubebuilder:validation:items:MaxLength=253
	// +kubebuilder:validation:XValidation:rule="self.all(x, self.filter(y, y == x).size() == 1)",message="read pools must be unique"
	// +listType=atomic
	Read []string `json:"read"`

	// Write lists pools writes fan out to. Required: the Lua schema
	// has no read-only keyspace.
	// +kubebuilder:validation:MinItems=1
	// +kubebuilder:validation:MaxItems=16
	// +kubebuilder:validation:items:MinLength=1
	// +kubebuilder:validation:items:MaxLength=253
	// +kubebuilder:validation:XValidation:rule="self.all(x, self.filter(y, y == x).size() == 1)",message="write pools must be unique"
	// +listType=atomic
	Write []string `json:"write"`

	// WritePolicy: "all" succeeds iff every write pool acks; "first"
	// succeeds on the first pool in Write order.
	// +optional
	// +kubebuilder:default=all
	// +kubebuilder:validation:Enum=all;first
	WritePolicy string `json:"writePolicy,omitempty"`

	// Merge selects the merge function for fan-out reads. Unset means
	// the gateway default ("first-hit") — the default lives Lua-side
	// next to the validator, so the renderer simply omits the key.
	// +optional
	Merge *MergeSpec `json:"merge,omitempty"`
}

// MergeSpec names the merge function a keyspace dispatches through,
// optionally carrying the WASM module implementing it.
type MergeSpec struct {
	// Name of a built-in merge or of a WASM module.
	// +kubebuilder:validation:MinLength=1
	// +kubebuilder:validation:Pattern=`^[A-Za-z0-9][A-Za-z0-9_-]*$`
	Name string `json:"name"`

	// Wasm optionally inlines the module implementing Name. Encoded
	// as base64 in YAML/JSON; rendered by the operator to
	// $MCGW_UDF_DIR/<Name>.wasm. The cap bounds the base64 form —
	// roughly 768 KiB of raw module — keeping the object well under
	// etcd's size limit; modules beyond it are the future
	// MergeFunction CRD's cue.
	// +optional
	// +kubebuilder:validation:MaxLength=1048576
	Wasm []byte `json:"wasm,omitempty"`
}
