package model_test

import (
	"encoding/json"
	"testing"

	"github.com/syaro/copysync/internal/model"
)

func TestTargetsJSONAll(t *testing.T) {
	b, err := json.Marshal(model.Targets{All: true})
	if err != nil || string(b) != `"all"` {
		t.Fatalf("marshal all = %s (err %v)", b, err)
	}
	var got model.Targets
	if err := json.Unmarshal([]byte(`"all"`), &got); err != nil || !got.All {
		t.Fatalf("unmarshal all => %+v (err %v)", got, err)
	}
}

func TestTargetsJSONList(t *testing.T) {
	b, err := json.Marshal(model.Targets{Devices: []model.DeviceID{"a", "b"}})
	if err != nil || string(b) != `["a","b"]` {
		t.Fatalf("marshal list = %s (err %v)", b, err)
	}
	var got model.Targets
	if err := json.Unmarshal([]byte(`["x","y"]`), &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if got.All || len(got.Devices) != 2 || got.Devices[0] != "x" {
		t.Fatalf("unmarshal list => %+v", got)
	}
}

func TestTargetsJSONRejectsGarbage(t *testing.T) {
	var got model.Targets
	if err := json.Unmarshal([]byte(`"everyone"`), &got); err == nil {
		t.Fatal("expected error for unknown string value")
	}
}

func TestClipEventRoundTrip(t *testing.T) {
	ev := model.ClipEvent{
		ID: "x", OriginDevice: "dev_1", Seq: 7, Mime: []string{"text/plain"},
		InlineText: "hi", Size: 2, Sha256: "deadbeef", Targets: model.Targets{All: true},
	}
	b, err := json.Marshal(ev)
	if err != nil {
		t.Fatal(err)
	}
	var got model.ClipEvent
	if err := json.Unmarshal(b, &got); err != nil {
		t.Fatal(err)
	}
	if got.InlineText != "hi" || !got.Targets.All || got.Seq != 7 {
		t.Fatalf("round trip mismatch: %+v", got)
	}
}
