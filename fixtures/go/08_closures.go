package closures

import "net/http"

// Handler serves traffic.
var Handler = func(w http.ResponseWriter, r *http.Request) {
	defer r.Body.Close()
	handle(w, r)
}

var transforms = []func(int) int{
	func(n int) int { return n * 2 },
	inc,
}

func inc(n int) int { return n + 1 }

func handle(w http.ResponseWriter, r *http.Request) {}

// Immediately runs a func literal at once.
func Immediately() string {
	return func() string { return "x" }()
}
