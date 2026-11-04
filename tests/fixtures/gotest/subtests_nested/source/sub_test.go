package corpus

import "testing"

func TestOuter(t *testing.T) {
	t.Run("Alpha", func(t *testing.T) {
		t.Run("Inner1", func(t *testing.T) {
			// passes
		})
		t.Run("Inner2 with spaces", func(t *testing.T) {
			t.Errorf("inner2 fails")
		})
	})
	t.Run("Beta/with/slash", func(t *testing.T) {
		t.Fatalf("beta forced fatal")
	})
}
