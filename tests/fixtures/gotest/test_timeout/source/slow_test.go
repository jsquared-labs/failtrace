package corpus

import (
	"testing"
	"time"
)

func TestQuick(t *testing.T) {
	if 1+1 != 2 {
		t.Fatal("nope")
	}
}

func TestSlow(t *testing.T) {
	time.Sleep(30 * time.Second) // exceeds -timeout=500ms
}
