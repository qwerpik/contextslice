package simple

import "errors"

// Login authenticates a user by name.
//
// It returns ErrMissing when the name is empty.
func Login(name string) (string, error) {
	if name == "" {
		return "", ErrMissing
	}
	return token(name), nil
}

func token(name string) string {
	return name + "-token"
}

var ErrMissing = errors.New("missing name")
