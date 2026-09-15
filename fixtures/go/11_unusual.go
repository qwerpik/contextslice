package unusual

func OneLiner() int { return 42 }

func WithSemicolons() int { x := 1; y := x + 1; return y }

func Ünicode() int { return 0 }

func ünicode() int { return 1 }

func init() { _ = OneLiner }

func main() {}

// Tagged carries struct tags.
type Tagged struct {
	A string `json:"a" xml:"a"`
}

// HardTokens exercises lexer corner cases and control flow.
func HardTokens() {
	s := `raw string with { braces } and "quotes"`
	const esc = "tab\there"
	ch := 'x'
Loop:
	for i := 0; i < 3; i++ {
		if i == 1 {
			continue Loop
		}
		break Loop
	}
	switch v := s.(type) {
	case string:
		_ = v
	default:
	}
	_ = esc
	_ = ch
}
