package vals

// Some is dot-imported.
func Some() int { return 1 }

// Conflict collides with a name in the dot-importing package.
func Conflict() int { return 2 }

// hidden is unexported and invisible to importers.
func hidden() {}
