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

type blobAuthReq struct {
	id    model.BlobID
	dev   model.DeviceID
	mode  BlobAuthMode
	reply chan bool
}

// blobACL records who may fetch a blob: the origin holder (the only device
// allowed to PUT it on demand) and the set of devices the referencing clip was
// routed to (origin ∪ targets).
type blobACL struct {
	origin  model.DeviceID
	allowed map[model.DeviceID]bool
}

// Hub owns the registry of connected clients and routes clips.
type Hub struct {
	store      Store
	log        *slog.Logger
	now        Clock
	serverID   string
	serverName string
	debug      bool // verbose connection-lifecycle logging (COPYSYNC_DEBUG=1)

	register   chan registerReq
	unregister chan *Client
	route      chan routeReq
	roster     chan rosterReq
	blobReq    chan blobReqMsg
	blobAuth   chan blobAuthReq
	setPool    chan setPoolReq
	evict      chan model.DeviceID

	clients  map[model.DeviceID]*Client      // owned by Run only
	onDemand map[model.BlobID]model.DeviceID // on-demand blobId -> origin holder; owned by Run
	// blobACL authorizes the blob channel: blobId -> the device set permitted to
	// fetch it (origin ∪ the referencing clip's targets), so a paired device can
	// only pull blobs it was actually a recipient of. Owned by Run.
	blobACL map[model.BlobID]blobACL

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
		blobAuth:   make(chan blobAuthReq),
		setPool:    make(chan setPoolReq),
		evict:      make(chan model.DeviceID),
		clients:    make(map[model.DeviceID]*Client),
		onDemand:   make(map[model.BlobID]model.DeviceID),
		blobACL:    make(map[model.BlobID]blobACL),
		monSubs:    make(map[int]chan MonitorEvent),
	}
}

// SetDebug toggles verbose connection-lifecycle logging. It is meant to be
// called once at startup (e.g. when COPYSYNC_DEBUG=1) before Run starts, so no
// synchronization is needed. Debug lines are emitted via the hub's logger,
// prefixed with "ws-debug" so they are easy to grep.
func (h *Hub) SetDebug(on bool) { h.debug = on }

// dbg emits a greppable verbose connection-lifecycle line when debug is enabled.
func (h *Hub) dbg(event string, args ...any) {
	if !h.debug {
		return
	}
	h.log.Info("ws-debug "+event, args...)
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
		case r := <-h.blobAuth:
			r.reply <- h.handleBlobAuth(r.id, r.dev, r.mode)
		case r := <-h.setPool:
			h.handleSetPool(r.id, r.pool)
			close(r.reply)
		case id := <-h.evict:
			h.handleEvict(id)
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

// Evict tears down any live connection for id and corrects presence. It is meant
// to be called after the device has been deleted or revoked from the store, so
// an already-open session is cut off immediately rather than continuing to relay
// clips and serve on-demand blobs until it happens to disconnect. No-op if the
// device has no live connection.
func (h *Hub) Evict(id model.DeviceID) { h.evict <- id }

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

// BlobAuthMode selects which authorization a blob-channel request needs.
type BlobAuthMode int

const (
	// BlobFetch authorizes a GET: dev must be a recorded recipient of the clip
	// that referenced the blob (origin ∪ targets), or — when no ACL is on record —
	// share the origin's pool.
	BlobFetch BlobAuthMode = iota
	// BlobSupply authorizes an on-demand PUT: dev must be the recorded origin
	// holder of the blob.
	BlobSupply
)

// AuthorizedForBlob reports whether dev is permitted to perform the given
// blob-channel operation on id. It is dispatched onto the Run goroutine so it
// reads the onDemand/blobACL maps without locking.
func (h *Hub) AuthorizedForBlob(id model.BlobID, dev model.DeviceID, mode BlobAuthMode) bool {
	reply := make(chan bool, 1)
	h.blobAuth <- blobAuthReq{id: id, dev: dev, reply: reply, mode: mode}
	return <-reply
}

func (h *Hub) handleBlobAuth(id model.BlobID, dev model.DeviceID, mode BlobAuthMode) bool {
	if mode == BlobSupply {
		// Only the device that advertised the on-demand blob may upload its bytes.
		origin, ok := h.onDemand[id]
		return ok && origin == dev
	}
	// Fetch path: dev must be in the referencing clip's recipient set.
	if acl, ok := h.blobACL[id]; ok {
		return acl.allowed[dev]
	}
	// No ACL recorded for this blob (e.g. it predates this process or was never
	// routed through here). Fall back to pool scoping against the on-demand
	// origin if we know one; otherwise deny.
	origin, ok := h.onDemand[id]
	if !ok {
		return false
	}
	return h.poolOf(origin) == h.poolOf(dev)
}

// pruneOnDemand drops every on-demand blob holding (and its ACL) owned by id and
// returns how many on-demand entries were removed. It ALSO reclaims blobACL
// entries for EAGER (non-on-demand) blobs originated by id: those never appear in
// h.onDemand, so without this they would accumulate for the whole process
// lifetime (a client-driven memory leak — e.g. a device streaming many distinct
// small images). Once the origin is offline its ACLs are dead weight (a fetch
// would have to come from a still-routed recipient, and the bytes are GC'd on
// TTL), so dropping them caps blobACL at the live origins' distinct blob sets.
func (h *Hub) pruneOnDemand(id model.DeviceID) int {
	pruned := 0
	for blobID, holder := range h.onDemand {
		if holder == id {
			delete(h.onDemand, blobID)
			delete(h.blobACL, blobID)
			pruned++
		}
	}
	// Reclaim any remaining (eager) ACLs whose origin is this now-offline device.
	for blobID, acl := range h.blobACL {
		if acl.origin == id {
			delete(h.blobACL, blobID)
		}
	}
	return pruned
}

func (h *Hub) handleBlobRequest(id model.BlobID) bool {
	origin, ok := h.onDemand[id]
	if !ok {
		h.dbg("blob_request-unknown", "blob", id)
		return false
	}
	c, online := h.clients[origin]
	if !online {
		h.dbg("blob_request-offline", "blob", id, "origin", origin)
		return false
	}
	b, err := protocol.Encode(protocol.TypeBlobReq, protocol.BlobRequest{ID: string(id)})
	if err != nil {
		h.log.Warn("blob_request encode failed", "blob", id, "origin", origin, "err", err)
		return false
	}
	if !c.Enqueue(b) {
		// Origin's send buffer is full: the blob-pull request is silently dropped
		// at the transport. Surface it (previously this returned false with no
		// trace, making on-demand pull failures impossible to diagnose).
		h.dbg("blob_request-drop", "blob", id, "origin", origin, "reason", "origin send buffer full")
		return false
	}
	return true
}

func (h *Hub) handleRegister(r registerReq) {
	c := r.client
	id := c.Device.ID
	if old, ok := h.clients[id]; ok {
		h.dbg("replace", "device", id, "reason", "replaced by a newer connection", "side", "server")
		old.Close("replaced by a newer connection")
		delete(h.clients, id)
	}
	h.clients[id] = c
	h.dbg("register", "device", id, "name", c.Device.Name, "pool", poolName(c.Device.Pool), "online_count", len(h.clients))
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
		settings, _ := h.store.GetSettings()
		for i, it := range items {
			b, err := protocol.Encode(protocol.TypeClip, it.Event)
			if err != nil {
				continue
			}
			if !c.Enqueue(b) {
				// Send buffer is full: DrainQueue already deleted the whole queue
				// bucket, so the undelivered tail exists only in this slice. Re-persist
				// it (preserving the original QueueItem so EnqueuedAt/TTL accounting
				// stays correct) before bailing; the next reconnect — or a slot freeing
				// up — re-drains it. Without this the tail is silently lost forever.
				for _, rem := range items[i:] {
					if _, err := h.store.Enqueue(id, rem, settings.QueueDepthPerDevice); err != nil {
						h.log.Warn("re-enqueue undelivered tail failed", "device", id, "err", err)
					}
				}
				break
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
		// Prune on-demand blob holdings owned by this device: once it is offline,
		// nobody can pull those blobs from it, so the entries are dead weight.
		// (Previously this map was never cleaned up and grew unbounded.)
		pruned := h.pruneOnDemand(id)
		h.dbg("unregister", "device", id, "reason", c.CloseReason(), "evicted", c.Evicted(), "online_count", len(h.clients), "ondemand_pruned", pruned, "ondemand_size", len(h.onDemand))
		h.broadcastPresence(c.Device, false, "")
	} else {
		// A stale unregister for a connection that was already replaced/evicted;
		// the current registry entry (if any) belongs to a newer connection.
		h.dbg("unregister-stale", "device", id, "reason", c.CloseReason(), "evicted", c.Evicted())
	}
}

func (h *Hub) handleEvict(id model.DeviceID) {
	c, ok := h.clients[id]
	if !ok {
		h.dbg("evict-by-id-absent", "device", id)
		return
	}
	c.Close("device deleted")
	delete(h.clients, id)
	// Drop any on-demand blob holdings owned by this device; nobody can pull them
	// from a connection that is being torn down.
	pruned := h.pruneOnDemand(id)
	h.dbg("evict-by-id", "device", id, "ondemand_pruned", pruned, "online_count", len(h.clients))
	// The store record is already gone, so announce offline using the client's
	// own device snapshot.
	h.broadcastPresence(c.Device, false, "")
}

func (h *Hub) handleRoute(ev model.ClipEvent) RouteResult {
	settings, _ := h.store.GetSettings()
	// Enforce E2E on ingest: when E2E is enabled the server must never see, store,
	// echo, or relay plaintext. A clip carrying inline plaintext (text or HTML)
	// with no encryption metadata violates that invariant — reject it before it
	// reaches the monitor feed or any peer. (The broadcast endpoint is the one
	// legitimate plaintext source, and it is already blocked when E2E is on.)
	if settings.E2EEnabled && ev.Enc == nil && (ev.InlineText != "" || ev.Html != "" || ev.BlobID != "") {
		// A blob-bearing clip with no encryption metadata means its referenced
		// payload is plaintext; under enforced E2E it must not be relayed/queued
		// or recorded as an on-demand holder (which would let peers pull the
		// plaintext bytes). Reject before any of that happens.
		h.log.Warn("rejected plaintext clip while E2E enforced", "device", ev.OriginDevice)
		return RouteResult{Status: protocol.AckRejected}
	}
	h.publishMonitor(ev, settings.E2EEnabled)
	_ = h.store.RecordActivity(h.now(), ev.Size)
	targets := h.resolveTargets(ev)
	// Authorize the blob channel against this clip: record the recipient set
	// (origin ∪ targets) so a later GET /blob/<id> is only served to a device the
	// clip was actually routed to (mirroring resolveTargets' pool/target scoping).
	if ev.BlobID != "" {
		// Refuse to let one device clobber a blob already claimed by a DIFFERENT,
		// still-connected origin: BlobID/OnDemand are client-controlled, so an
		// attacker could otherwise forge a clip referencing a victim's blob id to
		// take over its ACL (denying the real recipients) and its on-demand origin
		// (redirecting/withholding pulls). The legitimate holder keeps its claim
		// until it disconnects (pruneOnDemand) or re-registers the blob itself. The
		// clip itself is still relayed to its targets below; only the blob-supplier
		// bookkeeping is protected. claimOK gates both the ACL and on-demand writes.
		claimOK := true
		if cur, exists := h.blobACL[ev.BlobID]; exists && cur.origin != ev.OriginDevice {
			if _, online := h.clients[cur.origin]; online {
				h.dbg("blob-claim-rejected", "blob", ev.BlobID, "holder", cur.origin, "attempted_by", ev.OriginDevice)
				claimOK = false
			}
		}
		if claimOK {
			allowed := make(map[model.DeviceID]bool, len(targets)+1)
			allowed[ev.OriginDevice] = true
			for _, d := range targets {
				allowed[d.ID] = true
			}
			h.blobACL[ev.BlobID] = blobACL{origin: ev.OriginDevice, allowed: allowed}
			// Remember who holds an on-demand blob so a later GET can pull it from them.
			if ev.OnDemand {
				h.onDemand[ev.BlobID] = ev.OriginDevice
			}
		}
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
			// Slow client: disconnect and queue; it reconnects and drains. Reclaim
			// its on-demand blob holdings here too — handleUnregister will see the
			// registry entry already gone (stale branch) and skip pruning otherwise.
			h.dbg("evict", "device", dev.ID, "reason", "send buffer full", "side", "server")
			c.Close("send buffer full")
			delete(h.clients, dev.ID)
			h.pruneOnDemand(dev.ID)
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
	if ev.Targets.Broadcast {
		// Admin server broadcast: reach every device across ALL pools (unlike All,
		// which is pool-scoped via the origin's pool).
		devs, _ := h.store.ListDevices()
		return devs
	}
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

func (h *Hub) publishMonitor(ev model.ClipEvent, e2eEnforced bool) {
	// Treat the clip as opaque for preview purposes if it is encrypted OR E2E is
	// enforced server-wide: under enforced E2E no plaintext preview must ever enter
	// the monitor ring / SSE feed, even for a (non-conforming) clip with Enc==nil.
	e2e := ev.Enc != nil || e2eEnforced
	me := MonitorEvent{TS: ev.TS, Origin: h.deviceName(ev.OriginDevice), Pool: h.poolOf(ev.OriginDevice), Size: ev.Size, E2E: e2e}
	if len(ev.Mime) > 0 {
		me.Mime = ev.Mime[0]
	}
	switch {
	case ev.InlineText != "":
		me.Kind = "text"
		if me.E2E {
			me.Preview = "🔒 (E2E ciphertext)"
		} else {
			me.Preview = truncate(ev.InlineText, 16384) // full text (bounded); UI collapses + expands
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
