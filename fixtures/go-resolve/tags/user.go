package tags

// Use calls Decode: must bind to ALL four tag variants.
func Use() string {
	return Decode()
}
