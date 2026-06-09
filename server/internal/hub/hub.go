// Package hub is the real-time core: it owns the registry of connected clients
// and routes clip events to their targets, queueing for offline devices. All
// access to the client registry happens on a single goroutine (Run); every
// other caller communicates with it through channels, so the map needs no mutex.
package hub

import (
	"context"
	"log/slog"
	"strings"
	"sync"
	"time"

	"github.com/syaro/copysync/internal/config"
	"github.com/syaro/copysync/internal/model"
	"github.com/syaro/copysync/internal/protocol"
)

// Store is the persistence subset the hub needs.
type Store interface {
	ListDevices() ([]model.Device, error)
	GetDevice(model.DeviceID) (model.Device, bool, error)
	PutDevice(model.Device) error
	UpdateLastSeen(model.DeviceID, time.Time) error
	Enqueue(model.DeviceID, model.QueueItem, int) (int, error)
	DrainQueue(model.DeviceID) ([]model.QueueItem, error)
	GetSettings() (config.RuntimeSettings, error)
	RecordActivity(time.Time, int64) error
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

type setPoolReq struct {
	id    model.DeviceID
	pool  string
	reply chan struct{}
}

type rosterReq struct {
	reply chan []protocol.DeviceInfo
}

type blobReqMsg struct {
	id    model.BlobID
	reply chan bool
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
	blobReq    chan blobReqMsg
	setPool    chan setPoolReq

	clients  map[model.DeviceID]*Client      // owned by Run only
	onDemand map[model.BlobID]model.DeviceID // on-demand blobId -> origin holder; owned by Run

	monMu     sync.Mutex                // guards the live-monitor subscribers + ring
	monSubs   map[int]chan MonitorEvent // admin live-monitor subscribers
	monSeq    int
	monRecent []MonitorEvent // recent events replayed to a new subscriber
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
		blobReq:    make(chan blobReqMsg),
		setPool:    make(chan setPoolReq),
		clients:    make(map[model.DeviceID]*Client),
		onDemand:   make(map[model.BlobID]model.DeviceID),
		monSubs:    make(map[int]chan MonitorEvent),
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
		case r := <-h.blobReq:
			r.reply <- h.handleBlobRequest(r.id)
		case r := <-h.setPool:
			h.handleSetPool(r.id, r.pool)
			close(r.reply)
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

// RequestBlob asks the origin holder of an on-demand blob to upload it now.
// Returns false if the blob's origin is unknown or currently offline.
func (h *Hub) RequestBlob(id model.BlobID) bool {
	reply := make(chan bool, 1)
	h.blobReq <- blobReqMsg{id: id, reply: reply}
	return <-reply
}

func (h *Hub) handleBlobRequest(id model.BlobID) bool {
	origin, ok := h.onDemand[id]
	if !ok {
		return false
	}
	c, online := h.clients[origin]
	if !online {
		return false
	}
	b, err := protocol.Encode(protocol.TypeBlobReq, protocol.BlobRequest{ID: string(id)})
	if err != nil {
		return false
	}
	return c.Enqueue(b)
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
		ServerID:          h.serverID,
		ServerName:        h.serverName,
		E2E:               settings.E2EEnabled,
		You:               c.Device,
		Roster:            h.snapshot(),
		MaxMsg:            settings.MaxMessageBytes,
		BlobCap:           settings.BlobMaxBytes,
		OnDemandThreshold: settings.OnDemandThresholdBytes,
		Pools:             h.availablePools(settings),
		Pool:              poolName(c.Device.Pool),
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
	h.publishMonitor(ev)
	_ = h.store.RecordActivity(h.now(), ev.Size)
	targets := h.resolveTargets(ev)
	// Remember who holds an on-demand blob so a later GET can pull it from them.
	if ev.OnDemand && ev.BlobID != "" {
		h.onDemand[ev.BlobID] = ev.OriginDevice
	}
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
		// "all" routes to every device in the origin's share pool.
		devs, _ := h.store.ListDevices()
		pool := h.poolOf(ev.OriginDevice)
		out := make([]model.Device, 0, len(devs))
		for _, d := range devs {
			if poolName(d.Pool) == pool {
				out = append(out, d)
			}
		}
		return out
	}
	out := make([]model.Device, 0, len(ev.Targets.Devices))
	for _, id := range ev.Targets.Devices {
		if d, found, _ := h.store.GetDevice(id); found {
			out = append(out, d)
		}
	}
	return out
}

// poolName normalizes an empty pool to "default".
func poolName(p string) string {
	if p == "" {
		return "default"
	}
	return p
}

func (h *Hub) poolOf(id model.DeviceID) string {
	if d, found, _ := h.store.GetDevice(id); found {
		return poolName(d.Pool)
	}
	return "default"
}

// availablePools is the union of admin-configured pools and any pool a device
// currently belongs to, always including "default".
func (h *Hub) availablePools(settings config.RuntimeSettings) []string {
	seen := map[string]bool{}
	out := []string{}
	add := func(p string) {
		p = poolName(p)
		if !seen[p] {
			seen[p] = true
			out = append(out, p)
		}
	}
	add("default")
	for _, p := range settings.Pools {
		add(p)
	}
	if devs, err := h.store.ListDevices(); err == nil {
		for _, d := range devs {
			add(d.Pool)
		}
	}
	return out
}

// SetPool changes a device's share pool and refreshes the roster for everyone.
func (h *Hub) SetPool(id model.DeviceID, pool string) {
	reply := make(chan struct{})
	h.setPool <- setPoolReq{id: id, pool: poolName(pool), reply: reply}
	<-reply
}

func (h *Hub) handleSetPool(id model.DeviceID, pool string) {
	dev, found, err := h.store.GetDevice(id)
	if err != nil || !found || poolName(dev.Pool) == pool {
		return
	}
	dev.Pool = pool
	if err := h.store.PutDevice(dev); err != nil {
		h.log.Warn("set pool failed", "device", id, "err", err)
		return
	}
	if c, ok := h.clients[id]; ok {
		c.Device.Pool = pool
	}
	h.log.Info("device pool changed", "device", id, "pool", pool)
	h.broadcastPresence(dev, true, "")
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

// MonitorEvent is a live admin-monitor record of a relayed clip. Preview holds
// the inline text when visible (E2E off); E2E clips show only a marker.
type MonitorEvent struct {
	TS      string `json:"ts"`
	Origin  string `json:"origin"`
	Pool    string `json:"pool"`
	Kind    string `json:"kind"`
	Mime    string `json:"mime"`
	Size    int64  `json:"size"`
	Preview string `json:"preview"`
	BlobId  string `json:"blobId,omitempty"`
	E2E     bool   `json:"e2e"`
}

// SubscribeMonitor registers a live subscriber, returning its id, channel, and a
// snapshot of recent events to replay first.
func (h *Hub) SubscribeMonitor() (int, <-chan MonitorEvent, []MonitorEvent) {
	h.monMu.Lock()
	defer h.monMu.Unlock()
	id := h.monSeq
	h.monSeq++
	ch := make(chan MonitorEvent, 64)
	h.monSubs[id] = ch
	return id, ch, append([]MonitorEvent(nil), h.monRecent...)
}

// UnsubscribeMonitor removes a live subscriber.
func (h *Hub) UnsubscribeMonitor(id int) {
	h.monMu.Lock()
	defer h.monMu.Unlock()
	if ch, ok := h.monSubs[id]; ok {
		close(ch)
		delete(h.monSubs, id)
	}
}

func (h *Hub) publishMonitor(ev model.ClipEvent) {
	me := MonitorEvent{TS: ev.TS, Origin: h.deviceName(ev.OriginDevice), Pool: h.poolOf(ev.OriginDevice), Size: ev.Size, E2E: ev.Enc != nil}
	if len(ev.Mime) > 0 {
		me.Mime = ev.Mime[0]
	}
	switch {
	case ev.InlineText != "":
		me.Kind = "text"
		if me.E2E {
			me.Preview = "🔒 (E2E ciphertext)"
		} else {
			me.Preview = truncate(ev.InlineText, 200)
		}
	case ev.BlobID != "":
		if strings.HasPrefix(me.Mime, "image/") {
			me.Kind = "image"
		} else {
			me.Kind = "file"
		}
		me.Preview = ev.Name
		if !me.E2E {
			me.BlobId = string(ev.BlobID)
		}
	default:
		me.Kind = "text"
	}
	h.monMu.Lock()
	h.monRecent = append(h.monRecent, me)
	if len(h.monRecent) > 50 {
		h.monRecent = h.monRecent[len(h.monRecent)-50:]
	}
	for _, ch := range h.monSubs {
		select {
		case ch <- me:
		default: // drop for a slow subscriber
		}
	}
	h.monMu.Unlock()
}

func (h *Hub) deviceName(id model.DeviceID) string {
	if d, found, _ := h.store.GetDevice(id); found && d.Name != "" {
		return d.Name
	}
	return string(id)
}

func truncate(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	return string(r[:n]) + "…"
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
