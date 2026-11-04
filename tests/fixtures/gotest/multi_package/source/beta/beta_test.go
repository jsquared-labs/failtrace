package beta

import "testing"

func TestBetaPasses(t *testing.T) {}

func TestBetaFails(t *testing.T) {
	t.Errorf("beta package test failed")
}
