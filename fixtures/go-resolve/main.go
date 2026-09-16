package m

import (
	"fmt"
	fs "example.com/m/v2/internal/fs"
	"example.com/m/v2/auth"
	helpersvc "example.com/m/v2/dirhelper"
	"example.com/m/v2/types"
	"example.com/m/v2/nothere"
	"example.org/dep"
	"github.com/x/inner/v2"
	util "example.com/m/v2/mixed"
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
	inner.Thing() // external, major-version path: qualifier must be `inner`, not `v2`
	util.Mixed() // the dir also holds package main; the clause name wins
	fmt.Println(Version)
	ping()
	return nothere.Missing()
}

// Version is a package-level name for same-package binding.
var Version = "1"

func ping() {}
