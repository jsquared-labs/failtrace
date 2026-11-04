package gamma

import "testing"

func TestGammaPanics(t *testing.T) {
	var p *int
	_ = *p
	t.Log("unreachable")
}
