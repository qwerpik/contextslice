package meth

// A and B share the method name Name (ambiguous case); C has the unique Check.
type A struct{}

// Name is ambiguous across A and B.
func (a A) Name() string { return "a" }

// B also has Name.
type B struct{}

// Name on B.
func (b B) Name() string { return "b" }

// C is unique.
type C struct{}

// Check exists exactly once in the package.
func (c C) Check() bool { return true }

// Field is a plain struct field access target.
type S struct{ Field int }
