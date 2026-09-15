package constsvars

import "time"

// RetryCount is the number of attempts before giving up.
const RetryCount = 3

// Modes for the parser.
const (
	// ModeFast skips validation.
	ModeFast = iota
	ModeSlow
	_
	ModeAuto
)

var (
	// DefaultTimeout bounds one attempt.
	DefaultTimeout = 30 * time.Second
	verbose        bool
)

// Multi and Triple are set together.
var Multi, Triple = 2, 3
