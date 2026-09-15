package user

import . "example.com/m/v2/vals"

// Conflict is also declared here, so the dot-import collision is ambiguous.
func Conflict() int { return 3 }

// Use binds Some through the dot import; Conflict is ambiguous.
func Use() int {
	return Some() + Conflict()
}
