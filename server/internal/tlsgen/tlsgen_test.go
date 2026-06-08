package tlsgen_test

import (
	"testing"

	"github.com/syaro/copysync/internal/tlsgen"
)

func TestLoadOrCreatePinStable(t *testing.T) {
	dir := t.TempDir()
	r1, err := tlsgen.LoadOrCreate(dir, "Test", []string{"192.168.1.5", "copysync.local"})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	if r1.SPKIPin == "" {
		t.Fatal("empty SPKI pin")
	}
	// Loading again must reuse the persisted cert and yield the same pin.
	r2, err := tlsgen.LoadOrCreate(dir, "Test", nil)
	if err != nil {
		t.Fatalf("reload: %v", err)
	}
	if r1.SPKIPin != r2.SPKIPin {
		t.Fatalf("pin changed across reload: %s vs %s", r1.SPKIPin, r2.SPKIPin)
	}
	if r2.Certificate.Leaf == nil {
		t.Fatal("loaded certificate has no parsed leaf")
	}
}
