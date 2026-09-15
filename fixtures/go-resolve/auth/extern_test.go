package auth_test

import (
	"testing"

	"example.com/m/v2/auth"
)

// TestExternal binds only through the qualified import; it must never
// same-package-bind to auth's unexported names (the _test isolation rule).
func TestExternal(t *testing.T) {
	if err := auth.Login("u"); err != nil {
		t.Fatal(err)
	}
}
