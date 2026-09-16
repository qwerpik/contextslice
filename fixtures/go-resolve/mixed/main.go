package main

// A command sharing a directory with the real package: Go forbids
// importing it, so non_test_package must prefer `util`.

func main() {
	Mixed()
}
