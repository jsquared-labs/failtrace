package corpus

import "testing"

func TestPasses(t *testing.T) {
	if 1+1 != 2 {
		t.Fatal("nope")
	}
}

func TestFails(t *testing.T) {
	got := 2 + 2
	want := 5
	if got != want {
		t.Errorf("got %d, want %d", got, want)
	}
}

func TestAlsoFailsFatal(t *testing.T) {
	t.Fatalf("forced fatal: %s", "boom")
}
