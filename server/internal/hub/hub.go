// Package hub is the real-time core: it owns the registry of connected clients
// and routes clip events to their targets, queueing for offline devices. All
// access to the client registry happens on a single goroutine (Run); every
// other caller communicates with it through channels, so the map needs no mutex.
package hub

import (
	"context"
	"log/slog"
	"time"

	"github.com/syaro/copysync/internal/config"
	"github.com/syaro/copysync/internal/model"
	"github.com/syaro/copysync/internal/protocol"
)

// Store is the persistence subset the hub needs.
type Store interface {
	ListDevices() ([]model.Device, error)
	GetDevice(model.DeviceID) (model.Device, bool, error)
	UpdateLastSeen(model.DeviceID, time.Time) error
	Enqueue(model.DeviceID, model.QueueItem, int) (int, error)
	DrainQueue(model.DeviceID) ([]model.QueueItem, error)
	GetSettings() (config.RuntimeSettings, error)
}

// Clock returns the current time (overridable in tests).
type Clock func() time.Time

// RouteResult is returned to a sending client so it can build its ack.
type RouteResult struct {
	Status    string
	QueuedFor []model.DeviceID
}

type registerReq struct {
	client *Client
	reply  chan struct{}
}

type routeReq struct {
	ev    model.ClipEvent
	reply chan RouteResult
}

type rosterReq struct {
	reply chan []protocol.DeviceInfo
}

// Hub owns the registry of connected clients and routes clips.
type Hub struct {
	store      Store
	log        *slog.Logger
	now        Clock
	serverID   string
	serverName string

	register   chan registerReq
	unregister chan *Client
	route      chan routeReq
	roster     chan rosterReq

	clients map[model.DeviceID]*Client // owned by Run only
}

// New creates a hub.
func New(store Store, log *slog.Logger, now Clock, serverID, serverName string) *Hub {
	if now == nil {
		now = time.Now
	}
	return &Hub{
		store:      store,
		log:        log,
		now:        now,
		serverID:   serverID,
		serverName: serverName,
		register:   make(chan registerReq),
		unregister: make(chan *Client),
		route:      make(chan routeReq),
		roster:     make(chan rosterReq),
		clients:    make(map[model.DeviceID]*Client),
	}
}

// Run is the hub's event loop; it returns when ctx is cancelled.
func (h *Hub) Run(ctx context.Context) {
	for {
		select {
		case <-ctx.Done():
			return
		case r := <-h.register:
			h.handleRegister(r)
		case c := <-h.unregister:
			h.handleUnregister(c)
		case r := <-h.route:
			r.reply <- h.handleRoute(r.ev)
		case r := <-h.roster:
			r.reply <- h.snapshot()
		}
	}
}

// Register adds a client, sends it HelloOK followed by any queued clips, and
// announces its presence. It blocks until registration completes.
func (h *Hub) Register(c *Client) {
	reply := make(chan struct{})
	h.register <- registerReq{client: c, reply: reply}
	<-reply
}

// Unregister removes a client and announces that it went offline.
func (h *Hub) Unregister(c *Client) { h.unregister <- c }

// Route relays a clip to its targets and returns the disposition for an ack.
func (h *Hub) Route(ev model.ClipEvent) RouteResult {
	reply := make(chan RouteResult, 1)
	h.route <- routeReq{ev: ev, reply: reply}
	return <-reply
}

// Snapshot returns the current roster with online flags.
func (h *Hub) Snapshot() []protocol.DeviceInfo {
	reply := make(chan []protocol.DeviceInfo, 1)
	h.roster <- rosterReq{reply: reply}
	return <-reply
}

func (h *Hub) handleRegister(r registerReq) {
	c := r.client
	id := c.Device.ID
	if old, ok := h.clients[id]; ok {
		old.Close("replaced by a newer connection")
		delete(h.clients, id)
	}
	h.clients[id] = c
	_ = h.store.UpdateLastSeen(id, h.now())

	settings, _ := h.store.GetSettings()
	ok := protocol.HelloOK{
		ServerID:   h.serverID,
		ServerName: h.serverName,
		E2E:        settings.E2EEnabled,
		You:        c.Device,
		Roster:     h.snapshot(),
		MaxMsg:     settings.MaxMessageBytes,
		BlobCap:    settings.BlobMaxBytes,
	}
	if b, err := protocol.Encode(protocol.TypeHelloOK, ok); err == nil {
		c.Enqueue(b)
	}

	// Drain the offline queue (oldest first) before any live clip can arrive.
	if items, err := h.store.DrainQueue(id); err == nil {
		for _, it := range items {
			b, err := protocol.Encode(protocol.TypeClip, it.Event)
			if err != nil {
				continue
			}
			if !c.Enqueue(b) {
				break // buffer full; client will reconnect and we re-drain
			}
		}
	}

	h.broadcastPresence(c.Device, true, id)
	close(r.reply)
}

func (h *Hub) handleUnregister(c *Client) {
	id := c.Device.ID
	if cur, ok := h.clients[id]; ok && cur == c {
		delete(h.clients, id)
		_ = h.store.UpdateLastSeen(id, h.now())
		h.broadcastPresence(c.Device, false, "")
	}
}

func (h *Hub) handleRoute(ev model.ClipEvent) RouteResult {
	settings, _ := h.store.GetSettings()
	targets := h.resolveTargets(ev)
	var queuedFor []model.DeviceID
	relayedAny := false

	for _, dev := range targets {
		if dev.ID == ev.OriginDevice {
			continue // never echo back to the origin
		}
		if c, ok := h.clients[dev.ID]; ok {
			b, err := protocol.Encode(protocol.TypeClip, ev)
			if err != nil {
				continue
			}
			if c.Enqueue(b) {
				relayedAny = true
				continue
			}
			// Slow client: disconnect and queue; it reconnects and drains.
			c.Close("send buffer full")
			delete(h.clients, dev.ID)
		}
		h.enqueue(dev.ID, ev, settings.QueueDepthPerDevice)
		queuedFor = append(queuedFor, dev.ID)
	}

	status := protocol.AckRelayed
	if !relayedAny && len(queuedFor) > 0 {
		status = protocol.AckQueued
	}
	return RouteResult{Status: status, QueuedFor: queuedFor}
}

func (h *Hub) resolveTargets(ev model.ClipEvent) []model.Device {
	if ev.Targets.All {
		devs, _ := h.store.ListDevices()
		return devs
	}
	out := make([]model.Device, 0, len(ev.Targets.Devices))
	for _, id := range ev.Targets.Devices {
		if d, found, _ := h.store.GetDevice(id); found {
			out = append(out, d)
		}
	}
	return out
}

func (h *Hub) enqueue(id model.DeviceID, ev model.ClipEvent, depth int) {
	if _, err := h.store.Enqueue(id, model.QueueItem{Event: ev, EnqueuedAt: h.now()}, depth); err != nil {
		h.log.Warn("enqueue failed", "device", id, "err", err)
	}
}

func (h *Hub) snapshot() []protocol.DeviceInfo {
	devs, _ := h.store.ListDevices()
	out := make([]protocol.DeviceInfo, 0, len(devs))
	for _, d := range devs {
		_, online := h.clients[d.ID]
		out = append(out, protocol.DeviceInfo{Device: d, Online: online})
	}
	return out
}

func (h *Hub) broadcastPresence(d model.Device, online bool, exclude model.DeviceID) {
	b, err := protocol.Encode(protocol.TypePresence, protocol.Presence{Device: d, Online: online})
	if err != nil {
		return
	}
	for id, c := range h.clients {
		if id == exclude {
			continue
		}
		c.Enqueue(b)
	}
}
