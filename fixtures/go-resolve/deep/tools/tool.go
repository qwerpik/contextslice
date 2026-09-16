package tools

import "example.com/m/v2/deep/tools/internal/priv"

// Use may import the internal package: it sits inside the parent tree.
func Use() {
	priv.Grant()
}
