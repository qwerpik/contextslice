package auth

import "testing"

// TestLogin binds Login same-package (internal test file).
func TestLogin(t *testing.T) {
	s := &Session{User: "u"}
	if !s.Validate() {
		t.Fatal("invalid")
	}
	if err := Login("u"); err != nil {
		t.Fatal(err)
	}
}
