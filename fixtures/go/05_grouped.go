package grouped

type (
	// Pair holds two values of one type.
	Pair struct {
		First, Second int
	}
	// Key identifies a pair.
	Key string
)

// UsesLocals declares locals that must not become definitions.
func UsesLocals() {
	const localC = 1
	type localT struct{}
	var localV, localV2 int
	_ = localC
	_ = localV
	_ = localV2
	_ = localT{}
}
