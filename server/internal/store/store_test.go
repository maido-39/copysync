package store_test

import (
	"errors"
	"fmt"
	"testing"
	"time"

	"github.com/syaro/copysync/internal/model"
	"github.com/syaro/copysync/internal/store"
)

func openTest(t *testing.T) *store.Store {
	t.Helper()
	st, err := store.Open(t.TempDir())
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() { _ = st.Close() })
	return st
}

func TestQueueEnqueueDrainTrim(t *testing.T) {
	st := openTest(t)
	id := model.DeviceID("dev_x")
	for i := 1; i <= 5; i++ {
		ev := model.ClipEvent{ID: fmt.Sprintf("c%d", i), Seq: uint64(i)}
		if _, err := st.Enqueue(id, model.QueueItem{Event: ev, EnqueuedAt: time.Now()}, 3); err != nil {
			t.Fatal(err)
		}
	}
	if n, _ := st.QueueLen(id); n != 3 {
		t.Fatalf("len = %d, want 3 (trimmed)", n)
	}
	items, err := st.DrainQueue(id)
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"c3", "c4", "c5"} // oldest two trimmed, FIFO order kept
	if len(items) != len(want) {
		t.Fatalf("drained %d, want %d", len(items), len(want))
	}
	for i, it := range items {
		if it.Event.ID != want[i] {
			t.Fatalf("item %d = %s, want %s", i, it.Event.ID, want[i])
		}
	}
	if n, _ := st.QueueLen(id); n != 0 {
		t.Fatalf("after drain len = %d, want 0", n)
	}
}

func TestClaimPairingLifecycle(t *testing.T) {
	st := openTest(t)
	now := time.Now()
	mk := func(code string, exp time.Time) {
		if err := st.PutPairing(model.PairingCode{Code: code, CreatedAt: now, ExpiresAt: exp}); err != nil {
			t.Fatal(err)
		}
	}

	mk("12345678", now.Add(time.Minute))
	dev := model.Device{ID: "dev_1", Name: "A", CreatedAt: now, LastSeenAt: now}
	tok := model.TokenRecord{DeviceID: "dev_1", TokenHash: "h", IssuedAt: now}
	if err := st.ClaimPairing("12345678", now, dev, tok); err != nil {
		t.Fatalf("claim: %v", err)
	}
	if _, found, _ := st.GetDevice("dev_1"); !found {
		t.Fatal("device not persisted")
	}
	// reuse of consumed code fails
	if err := st.ClaimPairing("12345678", now, dev, tok); !errors.Is(err, store.ErrPairingInvalid) {
		t.Fatalf("reuse err = %v, want ErrPairingInvalid", err)
	}
	// duplicate name fails
	mk("22222222", now.Add(time.Minute))
	dup := model.Device{ID: "dev_2", Name: "A", CreatedAt: now, LastSeenAt: now}
	if err := st.ClaimPairing("22222222", now, dup, model.TokenRecord{DeviceID: "dev_2"}); !errors.Is(err, store.ErrNameTaken) {
		t.Fatalf("dup name err = %v, want ErrNameTaken", err)
	}
	// expired code fails
	mk("33333333", now.Add(-time.Minute))
	if err := st.ClaimPairing("33333333", now, model.Device{ID: "dev_3", Name: "C"}, model.TokenRecord{DeviceID: "dev_3"}); !errors.Is(err, store.ErrPairingInvalid) {
		t.Fatalf("expired err = %v, want ErrPairingInvalid", err)
	}
}

func TestDeviceNameUniquenessIndex(t *testing.T) {
	st := openTest(t)
	now := time.Now()
	_ = st.PutDevice(model.Device{ID: "dev_1", Name: "Phone", CreatedAt: now})
	if taken, _ := st.DeviceNameTaken("phone", "dev_2"); !taken {
		t.Fatal("expected name taken (case-insensitive)")
	}
	if taken, _ := st.DeviceNameTaken("phone", "dev_1"); taken {
		t.Fatal("same device should not count as taken")
	}
}

func TestAllQueuedBlobIDs(t *testing.T) {
	st := openTest(t)
	now := time.Now()
	_, _ = st.Enqueue("dev_a", model.QueueItem{Event: model.ClipEvent{ID: "1", BlobID: "sha256:aaa"}, EnqueuedAt: now}, 10)
	_, _ = st.Enqueue("dev_a", model.QueueItem{Event: model.ClipEvent{ID: "2"}, EnqueuedAt: now}, 10) // no blob
	_, _ = st.Enqueue("dev_b", model.QueueItem{Event: model.ClipEvent{ID: "3", BlobID: "sha256:bbb"}, EnqueuedAt: now}, 10)
	set, err := st.AllQueuedBlobIDs()
	if err != nil {
		t.Fatal(err)
	}
	if len(set) != 2 {
		t.Fatalf("set size = %d, want 2 (%v)", len(set), set)
	}
	if _, ok := set["sha256:aaa"]; !ok {
		t.Fatal("missing sha256:aaa")
	}
	if _, ok := set["sha256:bbb"]; !ok {
		t.Fatal("missing sha256:bbb")
	}
}

func TestPurgeExpiredQueueItems(t *testing.T) {
	st := openTest(t)
	now := time.Now()
	_, _ = st.Enqueue("dev_a", model.QueueItem{Event: model.ClipEvent{ID: "old"}, EnqueuedAt: now.Add(-2 * time.Hour)}, 10)
	_, _ = st.Enqueue("dev_a", model.QueueItem{Event: model.ClipEvent{ID: "new"}, EnqueuedAt: now}, 10)
	_, _ = st.Enqueue("dev_b", model.QueueItem{Event: model.ClipEvent{ID: "oldb"}, EnqueuedAt: now.Add(-2 * time.Hour)}, 10)
	n, err := st.PurgeExpiredQueueItems(now, time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	if n != 2 {
		t.Fatalf("purged = %d, want 2", n)
	}
	if l, _ := st.QueueLen("dev_a"); l != 1 {
		t.Fatalf("dev_a queue len = %d, want 1", l)
	}
	if l, _ := st.QueueLen("dev_b"); l != 0 {
		t.Fatalf("dev_b queue len = %d, want 0 (empty bucket dropped)", l)
	}
}

func TestSettingsRoundTripNormalize(t *testing.T) {
	st := openTest(t)
	s, _ := st.GetSettings()
	if s.MaxMessageBytes == 0 {
		t.Fatal("defaults not applied")
	}
	s.E2EEnabled = true
	s.AllowServerBroadcast = true // should be forced off by Normalize
	if err := st.PutSettings(s); err != nil {
		t.Fatal(err)
	}
	got, _ := st.GetSettings()
	if !got.E2EEnabled || got.AllowServerBroadcast {
		t.Fatalf("normalize failed: e2e=%v broadcast=%v", got.E2EEnabled, got.AllowServerBroadcast)
	}
}
