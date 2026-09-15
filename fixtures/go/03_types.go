package types

import "io"

// Store persists widgets.
type Store struct {
	WidgetCache map[string]Widget
	Limit       int
}

// Widget is the core domain type.
type Widget struct {
	ID   string
	Store // embedded store
	Tags []string `json:"tags"`
}

// Reader reads widgets.
type Reader interface {
	// Read returns the next n widgets.
	Read(n int) ([]Widget, error)
	io.Closer
}

// Celsius is a named type.
type Celsius float64

// Point is a type alias.
type Point = [2]float64
