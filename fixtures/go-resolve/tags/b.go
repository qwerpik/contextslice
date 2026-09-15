//go:build tagb

package tags

// Decode is defined once per build-tag variant (4-way duplicate).
func Decode() string { return "b" }

// Codec is the duplicate type.
type Codec struct{}
