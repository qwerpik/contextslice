package rel

import "./sub"

// Hi calls through a relative import.
func Hi() string {
	return sub.Hello()
}
