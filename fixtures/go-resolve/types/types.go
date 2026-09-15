package types

import "io"

// Widget is an in-repo type.
type Widget struct{ ID string }

// Reader embeds an external qualified type.
type Reader interface {
	io.Writer
}

// Use references types unqualified and qualified.
func Use() {
	var w Widget
	var r Reader
	_ = w
	_ = r
}
