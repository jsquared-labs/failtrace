package corpus

import (
	"sync"
	"testing"
)

func TestRaceyMap(t *testing.T) {
	m := make(map[int]int)
	var wg sync.WaitGroup
	for i := 0; i < 100; i++ {
		i := i
		wg.Add(1)
		go func() {
			defer wg.Done()
			m[i] = i * i
		}()
	}
	wg.Wait()
	if len(m) > 100 {
		t.Fatal("impossible")
	}
}
