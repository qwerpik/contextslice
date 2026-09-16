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

// D owns the unique Solo.
type D struct{}

// Solo exists exactly once in the package.
func (d D) Solo() string { return "d" }

// Run exists exactly once locally, but `t.Run` on an unknown receiver must
// stay unbound: Run is on testing.T's surface (the gin Engine.Run trap).
func (d D) Run(name string) {}

// Make returns a value: Make().Solo() chains through a computed operand.
func Make() *C { return nil }
