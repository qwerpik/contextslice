package outside

import "example.com/m/v2/deep/tools/internal/priv"

// Sneak imports an internal package it cannot see: the import is
// Unresolved(internal) and nothing may bind through it.
func Sneak() {
	priv.Grant()
}
