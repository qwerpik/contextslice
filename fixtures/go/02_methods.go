package methods

// Session holds login state.
type Session struct {
	// UserID is the external identifier.
	UserID string
	ttl    int
}

// Validate checks the session is still live.
func (s *Session) Validate() bool {
	return s.ttl > 0
}

// Refresh extends the session by delta.
func (s Session) Refresh(delta int) {
	s.ttl += delta
}

func (s *Session) private() {
	_ = s.UserID
}
