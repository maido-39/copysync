package store_test

import (
	"testing"
	"time"

	"github.com/syaro/copysync/internal/model"
	"github.com/syaro/copysync/internal/store"
)

func TestTokenRotationLifecycle(t *testing.T) {
	st, err := store.Open(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()

	now := time.Now()
	id := model.DeviceID("dev_rot")
	oldHash, newHash := "OLD_HASH", "NEW_HASH"

	// Seed a paired device whose token hash is in the index.
	if err := st.PutPairing(model.PairingCode{Code: "11112222", CreatedAt: now, ExpiresAt: now.Add(time.Hour)}); err != nil {
		t.Fatal(err)
	}
	dev := model.Device{ID: id, Name: "rot", Platform: "test", CreatedAt: now, LastSeenAt: now}
	if err := st.ClaimPairing("11112222", now, dev, model.TokenRecord{DeviceID: id, TokenHash: oldHash, IssuedAt: now}); err != nil {
		t.Fatal(err)
	}
	if gid, ok, _ := st.DeviceIDByTokenHash(oldHash); !ok || gid != id {
		t.Fatalf("old hash not indexed: %v %v", gid, ok)
	}

	// Rotate: both old and new resolve during the grace window.
	if err := st.RotateToken(id, newHash, now.Add(time.Minute)); err != nil {
		t.Fatal(err)
	}
	rec, _, _ := st.GetToken(id)
	if rec.TokenHash != newHash || rec.PrevHash != oldHash {
		t.Fatalf("rotate state wrong: %+v", rec)
	}
	if _, ok, _ := st.DeviceIDByTokenHash(oldHash); !ok {
		t.Fatal("old hash should still be valid during grace")
	}
	if _, ok, _ := st.DeviceIDByTokenHash(newHash); !ok {
		t.Fatal("new hash should be valid")
	}

	// A second rotate is a no-op while one is pending.
	if err := st.RotateToken(id, "NEWER", now.Add(2*time.Minute)); err != nil {
		t.Fatal(err)
	}
	if rec2, _, _ := st.GetToken(id); rec2.TokenHash != newHash || rec2.PrevHash != oldHash {
		t.Fatalf("second rotate should be a no-op: %+v", rec2)
	}

	// Retire: the old hash is invalidated; the new remains.
	if err := st.RetireOldToken(id); err != nil {
		t.Fatal(err)
	}
	if rec3, _, _ := st.GetToken(id); rec3.PrevHash != "" {
		t.Fatalf("prev hash should be cleared: %+v", rec3)
	}
	if _, ok, _ := st.DeviceIDByTokenHash(oldHash); ok {
		t.Fatal("old hash must be retired")
	}
	if _, ok, _ := st.DeviceIDByTokenHash(newHash); !ok {
		t.Fatal("new hash must remain valid")
	}

	// After retire, a fresh rotation is possible again.
	if err := st.RotateToken(id, "NEWER2", now.Add(3*time.Minute)); err != nil {
		t.Fatal(err)
	}
	if rec4, _, _ := st.GetToken(id); rec4.TokenHash != "NEWER2" || rec4.PrevHash != newHash {
		t.Fatalf("re-rotate after retire wrong: %+v", rec4)
	}
}
