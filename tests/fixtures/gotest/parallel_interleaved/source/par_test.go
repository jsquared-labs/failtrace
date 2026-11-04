package corpus

import (
	"testing"
	"time"
)

func TestParA(t *testing.T) {
	t.Parallel()
	time.Sleep(50 * time.Millisecond)
	t.Log("A done")
}

func TestParB(t *testing.T) {
	t.Parallel()
	time.Sleep(50 * time.Millisecond)
	t.Errorf("B fails after parallel")
}

func TestParC(t *testing.T) {
	t.Parallel()
	time.Sleep(50 * time.Millisecond)
	t.Log("C done")
}
