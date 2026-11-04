package corpus

import "testing"

func TestThatNeverCompiles(t *testing.T) {
	var x int = "this is not an int" // type mismatch
	_ = undefinedSymbol               // undeclared name
	t.Log(x)
}
