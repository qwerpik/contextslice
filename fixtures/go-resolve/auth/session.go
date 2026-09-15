package auth

import "errors"

// Login validates a session.
func Login(user string) error {
	if user == "" {
		return errors.New("empty")
	}
	return token(user)
}

func token(user string) string {
	return user
}

// Session carries state; Validate is the unique-method case.
type Session struct{ User string }

// Validate is the only method named Validate in this package.
func (s *Session) Validate() bool { return s.User != "" }
