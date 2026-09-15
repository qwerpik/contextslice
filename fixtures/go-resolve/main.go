package m

import (
	"fmt"
	fs "example.com/m/v2/internal/fs"
	"example.com/m/v2/auth"
	helpersvc "example.com/m/v2/dirhelper"
	"example.com/m/v2/types"
	"example.com/m/v2/nothere"
	"example.org/dep"
	_ "embed"
)

// Run exercises every import class the resolver must distinguish.
func Run() error {
	if err := auth.Login("u"); err != nil {
		return err
	}
	if !fs.Exists("/tmp") {
		return nil
	}
	helpersvc.H()
	var w types.Widget
	_ = w
	dep.Do()
	fmt.Println(Version)
	ping()
	return nothere.Missing()
}

// Version is a package-level name for same-package binding.
var Version = "1"

func ping() {}
