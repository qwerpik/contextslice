// Package docs demonstrates documentation handling.
// Second line of the package comment.
package docs

import "sync"

// This comment is separated from the function below by a blank line,
// so it is not a doc comment.

// Guarded returns the mutex.
// The second paragraph starts after an empty comment line.
//
// Third paragraph.
func Guarded() *sync.Mutex { return nil }

/*
Blocky is documented by a block comment.
Second line of the block.
*/
func Blocky() {}

func NoDoc() {}
