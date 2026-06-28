package hub

import (
	"sync"

	"github.com/syaro/copysync/internal/model"
)

// Client represents a single connected device from the hub's perspective. The
// transport layer owns the actual WebSocket connection and its read/write
// pumps; the hub only produces frames into Send and may signal eviction via
// Close. Send has multiple producers (the hub goroutine and the connection's
// own read pump) and a single consumer (the write pump), so it is never closed.
type Client struct {
	Device model.Device
	Send   chan []byte

	closeOnce   sync.Once
	done        chan struct{}
	closeReason string
	// evicted is true when Close was triggered by a server-side eviction or
	// replacement (an intentional, expected close) rather than a normal/error
	// teardown. The write pump uses this to pick a benign close code so clients
	// don't treat the disconnect as a protocol error.
	evicted bool
}

// NewClient creates a client with a buffered send channel.
func NewClient(d model.Device, sendBuffer int) *Client {
	return &Client{
		Device: d,
		Send:   make(chan []byte, sendBuffer),
		done:   make(chan struct{}),
	}
}

// Done is closed when the client should disconnect.
func (c *Client) Done() <-chan struct{} { return c.done }

// CloseReason returns why the client was evicted (empty if it left normally).
func (c *Client) CloseReason() string { return c.closeReason }

// Evicted reports whether this client was closed by an intentional server-side
// eviction/replacement (vs. a normal or error teardown). It is only meaningful
// after Done() has fired.
func (c *Client) Evicted() bool { return c.evicted }

// Close signals the transport to disconnect this client. Safe to call repeatedly.
// It marks the close as an intentional server-side eviction/replacement, so the
// write pump uses a benign WebSocket close code.
func (c *Client) Close(reason string) {
	c.closeOnce.Do(func() {
		c.closeReason = reason
		c.evicted = true
		close(c.done)
	})
}

// Enqueue tries to queue a frame for sending, returning false if the buffer is
// full (the caller then treats the client as too slow).
func (c *Client) Enqueue(b []byte) bool {
	select {
	case c.Send <- b:
		return true
	default:
		return false
	}
}
