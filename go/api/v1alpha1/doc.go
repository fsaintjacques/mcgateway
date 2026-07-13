// Package v1alpha1 contains API types for the mcgateway Kubernetes CRDs.
//
// The specs transcribe the gateway's Lua config schema
// (lua/mcgateway/config.lua is the normative validator); see
// doc/plans/stage-4-operator.md for the field mapping and the
// validation split between this schema, the operator's renderer, and
// the Lua validator.
//
// +groupName=mcgateway.dev
// +kubebuilder:object:generate=true
package v1alpha1
