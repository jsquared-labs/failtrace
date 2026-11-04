package corpus

import "testing"

func helper() {
	var p *int
	_ = *p // nil deref
}

func TestPasses(t *testing.T) {
	if 2+2 != 4 {
		t.Fatal("nope")
	}
}

func TestPanics(t *testing.T) {
	helper()
}
