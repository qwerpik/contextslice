package generics

// Number constrains numeric types.
type Number interface {
	~int | ~float64
}

// Pair is a generic pair.
type Pair[T any] struct {
	Left, Right T
}

// Map converts a slice of T into a slice of U.
func Map[T, U any](in []T, f func(T) U) []U {
	out := make([]U, 0, len(in))
	for _, v := range in {
		out = append(out, f(v))
	}
	return out
}

// First returns the left element.
func (p Pair[T]) First() T { return p.Left }

// Sum adds numbers.
func Sum[T Number](xs []T) T {
	var total T
	for _, x := range xs {
		total += x
	}
	return total
}
