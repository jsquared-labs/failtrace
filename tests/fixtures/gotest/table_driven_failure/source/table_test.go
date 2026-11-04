package corpus

import "testing"

func TestSquare(t *testing.T) {
	cases := []struct {
		name string
		in   int
		want int
	}{
		{"zero", 0, 0},
		{"one", 1, 1},
		{"two", 2, 4},
		{"three (wrong expected)", 3, 10},
		{"negative", -2, 4},
		{"big (wrong expected)", 9, 80},
	}
	for _, tc := range cases {
		tc := tc
		t.Run(tc.name, func(t *testing.T) {
			got := tc.in * tc.in
			if got != tc.want {
				t.Errorf("square(%d) = %d, want %d", tc.in, got, tc.want)
			}
		})
	}
}
