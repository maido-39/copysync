package httpapi

import "testing"

// The ring must stay bounded no matter how much is pushed — it is the server's
// side of the leak-hunt, so this is the load-bearing invariant.
func TestTelemetryRingBounded(t *testing.T) {
	r := newTelemetryRing(100)
	for i := 0; i < 1000; i++ {
		r.add([]TelemetryLine{{DeviceID: "d", Msg: "line"}})
	}
	got := r.recent(0, "")
	if len(got) != 100 {
		t.Fatalf("ring not bounded: got %d, want 100", len(got))
	}
}

func TestTelemetryRingKeepsNewest(t *testing.T) {
	r := newTelemetryRing(3)
	for _, m := range []string{"a", "b", "c", "d", "e"} {
		r.add([]TelemetryLine{{Msg: m}})
	}
	got := r.recent(0, "")
	if len(got) != 3 || got[0].Msg != "c" || got[2].Msg != "e" {
		t.Fatalf("expected newest [c d e], got %+v", got)
	}
}

func TestTelemetryRecentLimitAndFilter(t *testing.T) {
	r := newTelemetryRing(100)
	r.add([]TelemetryLine{
		{DeviceID: "a", Msg: "1"},
		{DeviceID: "b", Msg: "2"},
		{DeviceID: "a", Msg: "3"},
	})
	if got := r.recent(0, "a"); len(got) != 2 {
		t.Fatalf("device filter: got %d, want 2", len(got))
	}
	if got := r.recent(1, ""); len(got) != 1 || got[0].Msg != "3" {
		t.Fatalf("limit: got %+v, want newest single [3]", got)
	}
}

func TestSanitizeShort(t *testing.T) {
	if got := sanitizeShort("a\nb\tc", 100); got != "a b c" {
		t.Fatalf("newline/tab → space: got %q", got)
	}
	if got := sanitizeShort("ab\x00cd\x07", 100); got != "abcd" {
		t.Fatalf("control strip: got %q", got)
	}
	if got := sanitizeShort("abcdef", 3); got != "abc" {
		t.Fatalf("rune cap: got %q", got)
	}
}
