//go:build windows

package buildtag

import "syscall"

// Syscall is available on windows.
func Syscall() error { return syscall.EINVAL }
