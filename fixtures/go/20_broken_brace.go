package broken

// Works is complete and must be extracted.
func Works() int { return 1 }

// Dangling is missing its closing brace.
func Dangling() int {
	return 2
