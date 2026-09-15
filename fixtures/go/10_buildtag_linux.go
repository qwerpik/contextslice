//go:build linux

package buildtag

import "syscall"

// Syscall is available on linux.
func Syscall() error { return syscall.EINVAL }
