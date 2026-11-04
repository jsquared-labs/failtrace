package corpus

import "testing"

func init() {
	panic("init-time boom")
}

func TestNeverRuns(t *testing.T) {
	t.Fatal("unreachable")
}
