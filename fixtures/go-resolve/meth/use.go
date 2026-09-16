package meth

// Exercise hits every method/field binding rule.
func Exercise() {
	a := A{}
	b := B{}
	c := C{}
	s := S{}
	f := F{}
	_ = a.Name() // ambiguous: method_ambiguous
	_ = b.Name() // ambiguous: method_ambiguous
	_ = c.Check() // unique: binds to C.Check
	v := c.Check // method value: needs_type_info
	_ = s.Field // field access: needs_type_info
	_ = f.Close() // unique but universe name: unbound(universe_method)
	_ = Make().Solo() // chained through a computed operand: no_scope
	d := D{}
	_ = d.Solo() // identifier operand, unique: binds to D.Solo
	_ = t.Run("x", nil) // unknown receiver + testing-surface name: universe_method
	_ = v
	_ = f
	_ = d
}
