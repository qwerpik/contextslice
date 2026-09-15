package nested

import "example.com/m/v2/auth"

// Cross is inside a nested module: the import LOOKS in-repo but belongs to
// another module and must resolve as External.
func Cross() error {
	return auth.Login("u")
}
