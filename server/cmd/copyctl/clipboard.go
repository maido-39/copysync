package main

import (
	"bytes"
	"context"
	"os"
	"os/exec"
	"time"
)

// Clipboard abstracts the OS clipboard so the same sync loop runs on Wayland,
// X11, or headlessly (null backend) for testing.
type Clipboard interface {
	Name() string
	Read() (string, bool)
	Write(text string) error
	// Watch invokes onChange whenever the clipboard text changes, until ctx ends.
	Watch(ctx context.Context, onChange func(string))
}

// detectClipboard picks a backend from the environment and available tools.
func detectClipboard() Clipboard {
	if os.Getenv("WAYLAND_DISPLAY") != "" && have("wl-paste") && have("wl-copy") {
		return execClipboard{name: "wayland", readCmd: []string{"wl-paste", "-n"}, writeCmd: []string{"wl-copy"}}
	}
	if os.Getenv("DISPLAY") != "" && have("xclip") {
		return execClipboard{name: "x11", readCmd: []string{"xclip", "-selection", "clipboard", "-o"}, writeCmd: []string{"xclip", "-selection", "clipboard", "-i"}}
	}
	return nullClipboard{}
}

func have(bin string) bool {
	_, err := exec.LookPath(bin)
	return err == nil
}

// execClipboard shells out to wl-clipboard or xclip.
type execClipboard struct {
	name     string
	readCmd  []string
	writeCmd []string
}

func (e execClipboard) Name() string { return e.name }

func (e execClipboard) Read() (string, bool) {
	out, err := exec.Command(e.readCmd[0], e.readCmd[1:]...).Output()
	if err != nil {
		return "", false
	}
	return string(out), true
}

func (e execClipboard) Write(text string) error {
	cmd := exec.Command(e.writeCmd[0], e.writeCmd[1:]...)
	cmd.Stdin = bytes.NewReader([]byte(text))
	return cmd.Run()
}

func (e execClipboard) Watch(ctx context.Context, onChange func(string)) {
	pollWatch(ctx, e, onChange)
}

// nullClipboard is used on headless systems: it has no OS clipboard, so sync
// runs receive-only and Read never yields anything.
type nullClipboard struct{}

func (nullClipboard) Name() string         { return "none (headless)" }
func (nullClipboard) Read() (string, bool) { return "", false }
func (nullClipboard) Write(string) error   { return nil }
func (nullClipboard) Watch(ctx context.Context, _ func(string)) {
	<-ctx.Done()
}

// pollWatch detects changes by polling Read(); it skips the value present at
// startup so we don't immediately echo whatever was already on the clipboard.
func pollWatch(ctx context.Context, cb Clipboard, onChange func(string)) {
	var last string
	first := true
	ticker := time.NewTicker(800 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			cur, ok := cb.Read()
			if !ok {
				continue
			}
			if first {
				last, first = cur, false
				continue
			}
			if cur != last {
				last = cur
				onChange(cur)
			}
		}
	}
}
