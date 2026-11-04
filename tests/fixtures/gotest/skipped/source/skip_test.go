package corpus

import "testing"

func TestPasses(t *testing.T) {
	if 1+1 != 2 {
		t.Fatal("nope")
	}
}

func TestSkippedUnconditional(t *testing.T) {
	t.Skip("known issue, see #42")
}

func TestSkippedShort(t *testing.T) {
	if testing.Short() {
		t.Skip("skipping in short mode")
	}
}

func TestSkippedSkipNow(t *testing.T) {
	t.Logf("about to skip with SkipNow")
	t.SkipNow()
}
