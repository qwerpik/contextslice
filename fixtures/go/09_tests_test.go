package tests

import "testing"

func TestLogin(t *testing.T) {
	cases := []struct {
		name string
		want bool
	}{
		{name: "ok", want: true},
		{name: "missing", want: false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if !tc.want {
				t.Fatal("unexpected")
			}
		})
	}
}

func ExampleJoin() {
	// Output:
}

func BenchmarkParse(b *testing.B) {
	for b.Loop() {
	}
}

func helper() {}
