package imports

import (
	"fmt"
	f "strings"
	. "math"
	_ "embed"
)

// Join combines words with each import form in play.
func Join(words ...string) string {
	_ = f.Join
	_ = fmt.Sprint
	_ = Sqrt(2)
	return ""
}
